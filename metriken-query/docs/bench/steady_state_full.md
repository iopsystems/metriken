# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 326.5 ms | 20.5 ms | 9.3 ms | 241.5 ms | 1.7 ms | 239.5 ms |
| `AB_base_pin.parquet` | 3.1 | 316.3 ms | 10.1 ms | 2.7 ms | 226.9 ms | 0.1 ms | 239.8 ms |
| `AB_level.parquet` | 3.3 | 344.6 ms | 9.2 ms | 3.3 ms | 227.0 ms | 0.1 ms | 228.7 ms |
| `AB_level_pin.parquet` | 2.8 | 331.7 ms | 9.3 ms | 3.1 ms | 228.6 ms | 0.2 ms | 236.7 ms |
| `cachecannon.parquet` | 3.8 | 355.0 ms | 9.3 ms | 2.6 ms | 264.3 ms | 0.2 ms | 292.5 ms |
| `demo.parquet` | 1.2 | 111.0 ms | 8.9 ms | 2.5 ms | 104.8 ms | 0.1 ms | 118.1 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 759.6 ms | 11.2 ms | 3.4 ms | 1038.1 ms | 0.4 ms | 919.2 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2666.2 ms | 27.8 ms | 3.3 ms | 4312.6 ms | 1.2 ms | 4012.6 ms |
| `sglang_gemma3.parquet` | 2.1 | 295.5 ms | 23.2 ms | 2.8 ms | 481.4 ms | 0.7 ms | 480.5 ms |
| `vllm.parquet` | 4.0 | 363.4 ms | 19.5 ms | 3.1 ms | 440.7 ms | 0.7 ms | 473.6 ms |
| `vllm_gemma3.parquet` | 2.1 | 348.5 ms | 19.9 ms | 3.6 ms | 440.8 ms | 0.8 ms | 466.8 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.118 | 0.648 | 5.49× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.114 | 0.628 | 5.50× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.121 | 0.689 | 5.68× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.130 | 0.717 | 5.52× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.162 | 1.269 | 7.85× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.081 | 0.434 | 5.36× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.185 | 1.222 | 6.60× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.889 | 3.517 | 3.96× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.137 | 1.144 | 8.36× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.222 | 1.469 | 6.63× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.143 | 1.173 | 8.21× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.67 | 15.18× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 5.07 | 13.71× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.61 | 12.60× |
| `counter_ratio_scaled` | 1 | 4 | 0.19 | 2.41 | 12.39× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 2.17 | 12.18× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.42 | 12.06× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 2.73 | 11.68× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.65 | 11.59× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.01 | 11.59× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.46 | 11.30× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.66 | 10.58× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.27 | 9.38× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.79 | 9.32× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.21 | 9.05× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.96 | 8.15× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.86 | 8.12× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.20 | 1.57 | 7.82× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 0.98 | 7.43× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.30 | 7.07× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.40 | 6.52× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.42 | 2.61 | 6.28× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.32 | 4.82× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.14 | 0.39 | 2.87× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.17 | 2.01× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.10 | 1.92× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.16 | 1.81× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.09 | 1.41× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.13 | 0.02 | 0.17× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.04 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.08× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.49 | 13.31× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 4.36 | 12.37× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.67 | 12.03× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.57 | 11.98× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 1.99 | 11.73× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.68 | 11.64× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.95 | 11.49× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 2.19 | 10.98× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.76 | 10.81× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.95 | 10.55× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.64 | 9.86× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.02 | 9.21× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.64 | 8.71× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.71 | 8.07× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.04 | 7.98× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.04 | 7.85× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.07 | 7.81× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 2.76 | 6.96× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.31 | 6.92× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.45 | 6.27× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.18 | 0.93 | 5.30× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.30 | 4.80× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.19 | 2.44× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.11 | 2.39× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.34 | 2.32× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 2.08× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.09 | 1.38× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.02 | 0.36× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.03 | 0.34× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.65 | 14.67× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.69 | 12.33× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.39 | 4.73 | 12.02× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.70 | 12.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.26 | 3.07 | 11.90× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.74 | 11.50× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.27 | 10.67× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.46 | 10.62× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.18 | 1.88 | 10.20× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.01 | 10.17× |
| `counter_ratio_scaled` | 1 | 4 | 0.23 | 2.29 | 10.06× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.00 | 10.06× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.98 | 9.09× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.71 | 9.06× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.44 | 8.94× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.08 | 8.14× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.85 | 7.91× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.81 | 7.78× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.45 | 6.16× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.30 | 6.08× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.45 | 2.62 | 5.88× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.34 | 5.03× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.42 | 3.74× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.23 | 2.58× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.24 | 2.49× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.08 | 1.34× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.10 | 1.26× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.12 | 0.03 | 0.23× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.22× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.79 | 16.00× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 3.13 | 13.03× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.73 | 12.67× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.75 | 11.75× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.48 | 5.47 | 11.50× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.71 | 11.29× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.76 | 10.78× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.11 | 10.57× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.09 | 10.57× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.81 | 10.33× |
| `counter_ratio_scaled` | 1 | 4 | 0.26 | 2.48 | 9.50× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.13 | 9.04× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.44 | 8.99× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.26 | 8.87× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.83 | 7.80× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 2.91 | 6.76× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.34 | 6.54× |
| `counter_rate_bare_generic` | 2 | 8 | 0.18 | 1.17 | 6.50× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.42 | 6.30× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.20 | 1.13 | 5.71× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.31 | 1.64 | 5.30× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.35 | 5.09× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.48 | 3.04× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.24 | 2.50× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.24 | 2.34× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.10 | 2.03× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.11 | 0.10 | 0.87× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.33× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.33× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.03 | 0.26× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.17× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 10.81 | 23.26× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.71 | 11.14 | 15.68× |
| `counter_ratio_generic` | 2 | 8 | 0.62 | 8.85 | 14.25× |
| `counter_total_sum_generic` | 11 | 44 | 0.12 | 1.65 | 13.80× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.33 | 4.44 | 13.43× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.28 | 3.74 | 13.35× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.22 | 2.95 | 13.17× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.29 | 3.78 | 12.87× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.92 | 12.70× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.94 | 10.70× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.31 | 3.07 | 10.05× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.45 | 4.43 | 9.89× |
| `counter_ratio_scaled` | 1 | 4 | 0.59 | 5.48 | 9.30× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.75 | 6.92 | 9.18× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.46 | 12.73 | 8.74× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.75 | 8.71× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 1.28 | 8.06× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.48 | 8.05× |
| `counter_irate_total_mul` | 2 | 8 | 0.14 | 0.97 | 6.75× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.13 | 0.71 | 5.42× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.41 | 4.55× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.48 | 2.80× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.24 | 2.14× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.27 | 1.99× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.10 | 1.72× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.11 | 1.35× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.11 | 1.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.21× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.12 | 0.02 | 0.13× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.36 | 0.03 | 0.08× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.40 | 0.02 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.18 | 2.92 | 15.86× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.43 | 6.24 | 14.37× |
| `counter_ratio_generic` | 2 | 8 | 0.15 | 2.03 | 13.42× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.45 | 12.65× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.63 | 11.55× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.64 | 11.18× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.42 | 10.96× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 0.74 | 10.91× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.80 | 10.48× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.48 | 10.29× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.98 | 9.89× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.55 | 8.98× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.41 | 3.45 | 8.33× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.47 | 8.16× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 0.96 | 6.70× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.41 | 5.46× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.17 | 3.81× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.46 | 2.86× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.22 | 2.49× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.18 | 2.38× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.09 | 2.04× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.09 | 1.47× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.10 | 1.04× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.04 | 0.53× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.40× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.40× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.12 | 0.05 | 0.39× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.11× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.28 | 6.43 | 22.78× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.91 | 13.69× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.27 | 11.47× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.25 | 2.91 | 11.45× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.27 | 2.99 | 11.19× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.26 | 2.73 | 10.71× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 1.22 | 10.50× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.23 | 2.40 | 10.48× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.21 | 2.23 | 10.39× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.79 | 10.19× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.12 | 1.14 | 9.60× |
| `counter_total_sum_generic` | 11 | 44 | 0.17 | 1.54 | 9.34× |
| `counter_ratio_scaled` | 1 | 4 | 0.36 | 3.34 | 9.20× |
| `counter_irate_total_scaled` | 1 | 4 | 0.10 | 0.91 | 9.05× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.39 | 3.55 | 9.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.46 | 4.15 | 8.96× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.30 | 2.63 | 8.86× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.15 | 1.29 | 8.71× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.47 | 7.73× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.11 | 0.82 | 7.31× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.03 | 7.52 | 7.30× |
| `gauge_subtract` | 1 | 4 | 0.10 | 0.70 | 7.27× |
| `counter_rate_bare_generic` | 2 | 8 | 0.19 | 1.22 | 6.56× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.48 | 5.78× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.48 | 4.95× |
| `gauge_bare` | 10 | 40 | 0.09 | 0.43 | 4.80× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.25 | 2.09× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.21 | 1.99× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.12 | 1.97× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.11 | 1.47× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.12 | 1.34× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.09 | 1.17× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.03 | 0.29× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.09 | 0.02 | 0.25× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.12 | 0.03 | 0.22× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.10 | 0.02 | 0.20× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.12 | 0.02 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_generic` | 2 | 8 | 0.11 | 0.00 | 0.02× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.10 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.06 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.46 | 13.79 | 30.21× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.65 | 16.74 | 25.72× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.80 | 19.17 | 23.92× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.28 | 5.09 | 18.07× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.43 | 4.83 | 11.31× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.25 | 2.45 | 9.81× |
| `gauge_subtract` | 1 | 4 | 0.14 | 1.37 | 9.68× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.97 | 9.09 | 9.35× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.16 | 1.47 | 9.21× |
| `memory_util_pct` | 1 | 4 | 0.18 | 1.60 | 8.98× |
| `gauge_bare` | 10 | 40 | 0.10 | 0.87 | 8.96× |
| `counter_irate_total_scaled` | 1 | 4 | 0.18 | 1.54 | 8.45× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.80 | 6.63 | 8.25× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.16 | 1.29 | 8.04× |
| `counter_total_sum_generic` | 11 | 44 | 0.34 | 2.60 | 7.66× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.17 | 1.29 | 7.53× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.04 | 7.77 | 7.48× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.72 | 5.18 | 7.18× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.36 | 9.09 | 6.70× |
| `counter_irate_total_mul` | 2 | 8 | 0.21 | 1.36 | 6.61× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.06 | 6.96 | 6.56× |
| `counter_ratio_scaled` | 1 | 4 | 0.84 | 5.10 | 6.05× |
| `counter_ratio_generic` | 2 | 8 | 0.87 | 5.23 | 6.03× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.63 | 15.28 | 5.81× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 3.88 | 21.87 | 5.64× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.14 | 0.78 | 5.52× |
| `gauge_sum_scaled` | 1 | 4 | 0.11 | 0.60 | 5.39× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.52 | 4.95× |
| `counter_irate_by_id_scaled` | 2 | 8 | 4.89 | 22.49 | 4.60× |
| `softirq_irate_total_by_kind` | 10 | 40 | 1.00 | 4.51 | 4.50× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.41 | 1.55 | 3.75× |
| `counter_rate_bare_generic` | 2 | 8 | 0.63 | 2.25 | 3.55× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.34 | 7.93 | 3.39× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.29 | 2.24× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.10 | 0.18 | 1.85× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.10 | 0.17 | 1.63× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.16 | 0.23 | 1.46× |
| `gauge_sum_with_labels` | 5 | 20 | 0.10 | 0.14 | 1.34× |
| `gauge_sum_bare` | 2 | 8 | 0.14 | 0.18 | 1.31× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.39 | 0.48 | 1.22× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.17 | 0.02 | 0.11× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.00 | 0.01× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.13 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.18 | 3.53 | 19.80× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.30 | 5.38 | 17.93× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.35 | 5.58 | 16.00× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.18 | 2.73 | 15.39× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.97 | 15.36× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.07 | 1.08 | 15.16× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.20 | 2.92 | 14.48× |
| `counter_ratio_generic` | 2 | 8 | 0.28 | 3.94 | 14.14× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 1.02 | 13.00× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 2.55 | 12.72× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.66 | 8.40 | 12.67× |
| `counter_ratio_scaled` | 1 | 4 | 0.24 | 2.91 | 12.27× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.70 | 8.49 | 12.04× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.64 | 11.73× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.46 | 5.27 | 11.40× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.52 | 5.79 | 11.15× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.57 | 11.09× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 1.82 | 10.82× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.58 | 9.49× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.67 | 9.11× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.86 | 8.63× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.48 | 3.92 | 8.13× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.28 | 2.09 | 7.43× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 0.94 | 7.27× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.55 | 6.89× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.35 | 5.87× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.39 | 5.77× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.29 | 5.31× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.33 | 5.31× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.33 | 4.65× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.35 | 2.29× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.98× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.19 | 1.95× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.66× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.09 | 1.58× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.11 | 1.57× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.08 | 0.11 | 1.39× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.08 | 1.31× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.07 | 1.13× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 0.12 | 1.09× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.09 | 0.09 | 0.97× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.18× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.24 | 3.50 | 14.66× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.41 | 5.74 | 13.87× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 1.06 | 13.20 | 12.51× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.28 | 11.73× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.26 | 2.98 | 11.69× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.58 | 6.67 | 11.56× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.29 | 2.98 | 10.12× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.97 | 9.61 | 9.88× |
| `counter_total_sum_generic` | 11 | 44 | 0.20 | 1.88 | 9.59× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.24 | 2.20 | 9.32× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.69 | 9.18× |
| `memory_util_pct` | 1 | 4 | 0.09 | 0.78 | 9.12× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.13 | 1.13 | 8.57× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.20 | 1.70 | 8.49× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.18 | 1.45 | 8.26× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 1.20 | 7.77× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.74 | 5.71 | 7.76× |
| `counter_ratio_generic` | 2 | 8 | 0.63 | 4.82 | 7.63× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 0.98 | 7.35× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.91 | 6.66 | 7.35× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.77 | 5.38 | 7.03× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.55 | 6.78× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.47 | 6.47× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.74 | 4.33 | 5.87× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.18 | 1.03 | 5.65× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.14 | 0.77 | 5.50× |
| `gauge_avg_scaled` | 6 | 24 | 0.10 | 0.52 | 4.97× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.28 | 1.17 | 4.23× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.45 | 3.95× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.38 | 3.89× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.25 | 3.70× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.20 | 0.44 | 2.25× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.22 | 2.00× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.24 | 1.98× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.13 | 1.77× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.10 | 1.51× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.12 | 1.46× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.11 | 1.24× |
| `gauge_sum_bare` | 2 | 8 | 0.11 | 0.13 | 1.16× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.16 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.10 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.25 | 4.42 | 17.49× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.22 | 3.49 | 16.17× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.17 | 2.62 | 15.42× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.38 | 5.78 | 15.22× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.16 | 2.15 | 13.31× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 1.16 | 13.29× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 1.03 | 13.00× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 3.18 | 12.90× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.52 | 6.49 | 12.47× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.73 | 8.94 | 12.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.65 | 7.89 | 12.21× |
| `counter_ratio_scaled` | 1 | 4 | 0.23 | 2.69 | 11.53× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.82 | 11.50× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.62 | 11.36× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.41 | 11.15× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.66 | 11.00× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.22 | 2.35 | 10.83× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.70 | 10.62× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.49 | 5.16 | 10.49× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.24 | 10.09× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.62 | 5.13 | 8.29× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.92 | 8.22× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.97 | 8.12× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.52 | 7.96× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.85 | 7.94× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.51 | 3.71 | 7.21× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 0.98 | 6.84× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.36 | 5.81× |
| `gauge_subtract` | 1 | 4 | 0.11 | 0.60 | 5.25× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.34 | 5.25× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.36 | 4.96× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.34 | 4.72× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.31 | 4.71× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.20 | 2.15× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.37 | 2.14× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.99× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.52× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.09 | 1.52× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.08 | 1.31× |
| `gauge_sum_bare` | 2 | 8 | 0.10 | 0.11 | 1.10× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.07 | 0.98× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.34× | 0.11 | 1.68 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 13.71× | 0.37 | 5.07 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 13.59× | 0.10 | 1.42 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.09× | 0.12 | 1.54 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 12.99× | 0.20 | 2.59 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 12.60× | 0.13 | 1.61 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 12.59× | 0.12 | 1.46 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 12.39× | 0.19 | 2.41 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 12.18× | 0.18 | 2.17 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 12.02× | 0.14 | 1.67 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="decode_forward",source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.99× | 0.11 | 1.69 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.33× | 0.07 | 0.94 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.31× | 0.11 | 1.49 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 12.45× | 0.11 | 1.39 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.37× | 0.35 | 4.36 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 12.03× | 0.22 | 2.67 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 11.98× | 0.13 | 1.57 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 11.97× | 0.11 | 1.28 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 11.73× | 0.17 | 1.99 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 11.64× | 0.06 | 0.68 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_http_request_duration_seconds{source="sglang-router"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.33× | 0.10 | 1.58 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 15.24× | 0.11 | 1.72 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.13× | 0.07 | 1.01 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.72× | 0.11 | 1.60 | `counter_total_sum_generic` | `sum(irate(cpu_tlb_flush[5m]))` |
| 13.21× | 0.13 | 1.66 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.91× | 0.13 | 1.65 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 12.55× | 0.12 | 1.45 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 12.33× | 0.06 | 0.69 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.02× | 0.39 | 4.73 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 12.01× | 0.22 | 2.70 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.05 | 0.00 | `gauge_bare` | `tcp_packet_latency` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.93× | 0.11 | 2.12 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.38× | 0.07 | 1.09 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.96× | 0.15 | 2.12 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 13.56× | 0.12 | 1.66 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.42× | 0.06 | 0.82 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 13.38× | 0.13 | 1.79 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 13.21× | 0.10 | 1.36 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 13.03× | 0.24 | 3.13 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 12.74× | 0.13 | 1.63 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.67× | 0.06 | 0.73 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.03 | 0.00 | `gauge_avg_scaled` | `avg(gpu_tensor_utilization) / 100` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 30.84× | 0.15 | 4.71 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 25.19× | 0.12 | 2.95 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 23.26× | 0.46 | 10.81 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.65× | 0.34 | 6.97 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 19.82× | 0.38 | 7.56 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 19.02× | 0.38 | 7.24 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 18.88× | 0.39 | 7.41 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 18.65× | 0.38 | 7.18 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 18.61× | 0.09 | 1.65 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 17.66× | 0.39 | 6.84 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.00× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.00× | 0.05 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.52× | 0.10 | 1.71 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 15.86× | 0.18 | 2.92 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 15.48× | 0.07 | 1.05 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.37× | 0.43 | 6.24 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.31× | 0.12 | 1.76 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.92× | 0.13 | 1.85 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.42× | 0.15 | 2.03 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 13.40× | 0.09 | 1.26 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 13.33× | 0.15 | 1.95 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 13.28× | 0.18 | 2.36 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.07 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.13 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 35.63× | 0.08 | 2.90 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 22.78× | 0.28 | 6.43 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 17.36× | 0.13 | 2.21 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 16.55× | 0.11 | 1.83 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 15.72× | 0.13 | 2.10 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 15.09× | 0.14 | 2.11 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 14.82× | 0.13 | 1.91 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 14.67× | 0.15 | 2.15 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 13.91× | 0.10 | 1.37 | `counter_irate_ratio_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source…` |
| 13.89× | 0.24 | 3.31 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.18 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_generation_duration_seconds{source="sglang-router"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 30.21× | 0.46 | 13.79 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 25.72× | 0.65 | 16.74 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 25.13× | 0.20 | 5.12 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 23.92× | 0.80 | 19.17 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 21.21× | 0.43 | 9.09 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 19.81× | 0.28 | 5.58 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 17.12× | 0.34 | 5.84 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 16.85× | 0.35 | 5.85 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 16.63× | 0.31 | 5.09 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 16.48× | 0.31 | 5.17 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.25 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.07 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_generation_duration_seconds{source="sglang-router"}` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 22.52× | 0.16 | 3.57 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 21.84× | 0.17 | 3.80 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 21.33× | 0.12 | 2.64 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 19.91× | 0.18 | 3.53 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 19.56× | 0.18 | 3.58 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 19.22× | 0.18 | 3.43 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 19.17× | 0.35 | 6.68 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 18.73× | 0.31 | 5.79 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 18.71× | 0.18 | 3.34 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="local_mm_shootdown"}[5m]))` |
| 18.65× | 0.27 | 5.04 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.13 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.74× | 0.28 | 4.63 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 16.44× | 0.20 | 3.37 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 16.38× | 0.28 | 4.59 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 16.04× | 0.24 | 3.82 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 15.61× | 0.22 | 3.43 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 15.31× | 0.27 | 4.16 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 15.27× | 0.24 | 3.65 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 14.70× | 0.20 | 2.97 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 14.66× | 0.24 | 3.50 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 14.63× | 0.23 | 3.39 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.24 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 0.01× | 0.16 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.12 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-decode"}` |
| 0.01× | 0.12 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang"}` |
| 0.01× | 0.15 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 22.57× | 0.18 | 4.01 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 19.73× | 0.19 | 3.71 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 19.37× | 0.17 | 3.38 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 19.11× | 0.18 | 3.44 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 18.94× | 0.19 | 3.54 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 18.45× | 0.20 | 3.76 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 18.23× | 0.35 | 6.39 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 18.22× | 0.36 | 6.49 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 18.16× | 0.19 | 3.38 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 18.02× | 0.15 | 2.70 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |

