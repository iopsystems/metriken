# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 340.5 ms | 24.0 ms | 6.7 ms | 289.5 ms | 1.8 ms | 254.2 ms |
| `AB_base_pin.parquet` | 3.1 | 323.5 ms | 11.5 ms | 3.7 ms | 235.0 ms | 0.1 ms | 234.0 ms |
| `AB_level.parquet` | 3.3 | 330.4 ms | 10.7 ms | 2.9 ms | 226.5 ms | 0.1 ms | 233.0 ms |
| `AB_level_pin.parquet` | 2.8 | 329.3 ms | 11.1 ms | 2.3 ms | 221.5 ms | 0.2 ms | 228.9 ms |
| `cachecannon.parquet` | 3.8 | 358.9 ms | 10.2 ms | 2.5 ms | 260.7 ms | 0.2 ms | 268.4 ms |
| `demo.parquet` | 1.2 | 110.3 ms | 12.3 ms | 3.3 ms | 97.0 ms | 0.2 ms | 110.2 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 737.3 ms | 11.1 ms | 4.1 ms | 916.9 ms | 0.4 ms | 1044.8 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2650.6 ms | 28.8 ms | 3.4 ms | 4861.0 ms | 0.9 ms | 3599.9 ms |
| `sglang_gemma3.parquet` | 2.1 | 296.5 ms | 23.8 ms | 3.6 ms | 426.5 ms | 0.8 ms | 453.9 ms |
| `vllm.parquet` | 4.0 | 379.2 ms | 19.0 ms | 2.9 ms | 431.6 ms | 0.7 ms | 446.2 ms |
| `vllm_gemma3.parquet` | 2.1 | 330.2 ms | 18.9 ms | 3.9 ms | 473.7 ms | 0.6 ms | 479.6 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.127 | 1.069 | 8.39× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.127 | 1.050 | 8.30× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.128 | 1.051 | 8.18× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.129 | 1.094 | 8.48× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.164 | 1.460 | 8.91× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.084 | 0.531 | 6.35× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.189 | 1.628 | 8.62× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.897 | 6.730 | 7.51× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.157 | 1.521 | 9.68× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.222 | 2.029 | 9.14× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.159 | 1.500 | 9.45× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 6.14 | 16.69× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.76 | 15.78× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.14 | 1.87 | 13.78× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.72 | 13.44× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.86 | 12.74× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.15 | 12.20× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.74 | 12.16× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 1.39 | 11.97× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.42 | 11.85× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.70 | 11.71× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.23 | 2.68 | 11.58× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 1.70 | 11.49× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.28 | 3.16 | 11.35× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.82 | 10.46× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.23 | 2.31 | 9.98× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 1.04 | 9.95× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.45 | 9.63× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 1.11 | 9.14× |
| `counter_ratio_generic` | 2 | 8 | 0.24 | 2.07 | 8.61× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.66 | 8.35× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.40 | 7.35× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.34 | 7.03× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 2.76 | 7.02× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.43 | 2.67× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.21 | 2.13× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.23 | 2.11× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.09 | 1.30× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.26× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.25× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.22× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.14 | 0.03 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.71 | 15.25× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 5.07 | 14.30× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.79 | 14.10× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 3.13 | 13.24× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.77 | 12.99× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.42 | 12.40× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 3.10 | 12.38× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.97 | 11.52× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.46 | 11.00× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.17 | 1.80 | 10.35× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.36 | 10.34× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.02 | 10.33× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.04 | 10.29× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.15 | 10.28× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.35 | 9.23× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.24 | 2.20 | 9.00× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.15 | 1.15 | 7.75× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.69 | 7.45× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 1.12 | 7.19× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.38 | 7.10× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.81 | 6.79× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.30 | 6.77× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 2.80 | 6.56× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.41 | 3.53× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.20 | 2.31× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.23 | 1.69× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.11 | 1.47× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.03 | 0.33× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.22× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.13 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 3.00 | 14.68× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.45 | 6.21 | 13.90× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.13 | 1.73 | 13.54× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.63 | 12.36× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 1.51 | 12.30× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 3.03 | 12.10× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.14 | 1.65 | 12.07× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.23 | 2.66 | 11.77× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.80 | 11.33× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.88 | 11.16× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.69 | 11.04× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.59 | 10.25× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 0.99 | 9.54× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.97 | 9.41× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.21 | 1.91 | 9.27× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.45 | 9.02× |
| `counter_ratio_generic` | 2 | 8 | 0.24 | 1.99 | 8.43× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.63 | 7.72× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.80 | 7.61× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 1.01 | 7.50× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.36 | 6.96× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 2.82 | 6.34× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.32 | 6.28× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.42 | 3.17× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.25 | 2.82× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.20 | 2.17× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.11 | 0.10 | 0.90× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.32× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.32× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.02 | 0.23× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.13 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.39 | 5.83 | 15.05× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.13 | 1.75 | 13.99× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.78 | 13.22× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.75 | 12.98× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.48 | 12.04× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.71 | 11.97× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.65 | 11.97× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 2.87 | 11.67× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.23 | 2.66 | 11.63× |
| `counter_ratio_scaled` | 1 | 4 | 0.23 | 2.43 | 10.40× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.07 | 9.95× |
| `counter_total_sum_generic` | 11 | 44 | 0.11 | 1.13 | 9.94× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.76 | 9.71× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 2.12 | 9.64× |
| `counter_ratio_generic` | 2 | 8 | 0.22 | 2.05 | 9.14× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.50 | 8.49× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.71 | 7.82× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.33 | 7.45× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 1.12 | 7.22× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.77 | 6.71× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.16 | 1.08 | 6.65× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 2.87 | 6.46× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.28 | 5.47× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.50 | 3.23× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.26 | 2.75× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.22 | 2.34× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.09 | 1.25× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.02 | 0.41× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.13 | 0.02 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.13 | 0.02 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 1.31 | 41.34 | 31.67× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.42 | 9.09 | 21.46× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.25 | 4.15 | 16.46× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.70 | 10.20 | 14.57× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.32 | 4.53 | 13.98× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.29 | 4.00 | 13.89× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.20 | 2.63 | 13.35× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.73 | 9.19 | 12.65× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.26 | 3.19 | 12.05× |
| `counter_ratio_generic` | 2 | 8 | 0.61 | 6.88 | 11.34× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.84 | 11.33× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 1.09 | 9.53× |
| `counter_ratio_scaled` | 1 | 4 | 0.57 | 5.29 | 9.30× |
| `counter_total_sum_generic` | 11 | 44 | 0.17 | 1.59 | 9.14× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.50 | 4.37 | 8.76× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.78 | 8.43× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.88 | 8.16× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 1.20 | 8.05× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.78 | 7.53× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.14 | 1.05 | 7.28× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.40 | 7.20× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.49 | 7.05× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.52 | 3.25× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.24 | 2.24× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.25 | 1.72× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.12 | 0.11 | 0.94× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.11 | 0.90× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.09 | 0.02 | 0.21× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.12 | 0.02 | 0.13× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.25 | 0.02 | 0.09× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.29 | 0.02 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.11 | 0.00 | 0.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.10 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.38 | 6.86 | 17.99× |
| `counter_irate_total_scaled` | 1 | 4 | 0.05 | 0.85 | 15.73× |
| `counter_ratio_generic` | 2 | 8 | 0.15 | 2.16 | 14.87× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.85 | 13.86× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.46 | 13.46× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.79 | 13.43× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.64 | 13.03× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.52 | 12.10× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.60 | 11.23× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.78 | 10.49× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.84 | 10.26× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.37 | 10.24× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 0.72 | 10.11× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.56 | 9.72× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 3.38 | 8.37× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.36 | 7.38× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.87 | 7.35× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.19 | 4.64× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.20 | 2.71× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.41 | 2.64× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.24 | 2.53× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.09 | 1.40× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.06 | 0.08 | 1.32× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.41× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.02 | 0.31× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.21× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.12 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.11× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.09× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.05 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 9.48 | 24.83× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.01 | 19.89 | 19.61× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.24 | 3.05 | 12.61× |
| `counter_ratio_scaled` | 1 | 4 | 0.33 | 4.08 | 12.51× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.26 | 3.20 | 12.17× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.72 | 11.55× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 1.27 | 11.33× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.22 | 2.49 | 11.29× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.26 | 2.87 | 11.20× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.92 | 10.87× |
| `counter_total_sum_generic` | 11 | 44 | 0.13 | 1.35 | 10.53× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.40 | 3.98 | 10.07× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.13 | 1.20 | 9.45× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.10 | 0.94 | 9.27× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.27 | 2.49 | 9.08× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.12 | 1.00 | 8.68× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.85 | 8.38× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.24 | 1.96 | 8.11× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.44 | 3.49 | 7.96× |
| `gauge_subtract` | 1 | 4 | 0.10 | 0.72 | 7.48× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.16 | 1.18 | 7.45× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.88 | 7.39× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.43 | 6.37× |
| `counter_rate_bare_generic` | 2 | 8 | 0.19 | 1.22 | 6.37× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.50 | 5.76× |
| `gauge_sum_bare` | 2 | 8 | 0.10 | 0.49 | 5.08× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.43 | 5.03× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.45 | 5.03× |
| `gauge_bare` | 10 | 40 | 0.08 | 0.40 | 4.91× |
| `gauge_max_bare` | 1 | 4 | 0.12 | 0.42 | 3.67× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.21 | 1.79× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.18 | 1.70× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.03 | 0.30× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.11 | 0.03 | 0.24× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.09 | 0.02 | 0.22× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.14 | 0.03 | 0.20× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.15 | 0.02 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.01 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.00 | 0.02× |
| `counter_ratio_generic` | 2 | 8 | 0.11 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.00 | 0.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.11 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns` | 1 | 4 | 0.36 | 19.43 | 53.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.25 | 10.29 | 41.22× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.86 | 19.53 | 22.58× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.27 | 4.76 | 17.67× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.42 | 69.25 | 12.79× |
| `counter_irate_total_scaled` | 1 | 4 | 0.19 | 2.09 | 11.05× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.45 | 4.77 | 10.68× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.99 | 10.44 | 10.54× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.81 | 7.75 | 9.59× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.79 | 7.52 | 9.51× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.14 | 1.25 | 8.73× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.18 | 1.52 | 8.60× |
| `gauge_subtract` | 1 | 4 | 0.15 | 1.26 | 8.57× |
| `memory_util_pct` | 1 | 4 | 0.18 | 1.45 | 8.25× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.08 | 8.68 | 8.07× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.67 | 5.24 | 7.83× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.01 | 7.88 | 7.81× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.16 | 1.25 | 7.67× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.38 | 18.22 | 7.64× |
| `counter_total_sum_generic` | 11 | 44 | 0.37 | 2.81 | 7.59× |
| `gauge_bare` | 10 | 40 | 0.11 | 0.83 | 7.39× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.26 | 1.91 | 7.28× |
| `counter_ratio_generic` | 2 | 8 | 0.80 | 5.32 | 6.66× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.21 | 1.32 | 6.34× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.13 | 0.77 | 5.85× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.12 | 0.68 | 5.81× |
| `counter_irate_total_mul` | 2 | 8 | 0.21 | 1.22 | 5.74× |
| `gauge_max_bare` | 1 | 4 | 0.10 | 0.55 | 5.71× |
| `counter_ratio_scaled` | 1 | 4 | 0.85 | 4.65 | 5.45× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.43 | 7.74 | 5.42× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.81 | 14.87 | 5.30× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 4.12 | 20.11 | 4.88× |
| `gauge_sum_bare` | 2 | 8 | 0.13 | 0.64 | 4.85× |
| `gauge_sum_with_labels` | 5 | 20 | 0.12 | 0.55 | 4.70× |
| `gauge_sum_scaled` | 1 | 4 | 0.13 | 0.58 | 4.36× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.41 | 1.51 | 3.73× |
| `counter_rate_bare_generic` | 2 | 8 | 0.61 | 2.02 | 3.29× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.23 | 1.52× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.15 | 0.21 | 1.46× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.31 | 0.43 | 1.36× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.09 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.11 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.49 | 12.92 | 26.26× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.45 | 8.12 | 18.01× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.60 | 10.22 | 17.00× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.35 | 5.70 | 16.17× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.22 | 3.57 | 15.96× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.21 | 3.36 | 15.93× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.19 | 2.87 | 15.34× |
| `counter_ratio_scaled` | 1 | 4 | 0.22 | 3.30 | 15.04× |
| `counter_ratio_generic` | 2 | 8 | 0.29 | 4.35 | 14.92× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.26 | 3.93 | 14.83× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.38 | 5.29 | 13.85× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 1.08 | 12.99× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.87 | 12.91× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.09 | 1.10 | 12.88× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.75 | 12.51× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.80 | 9.74 | 12.12× |
| `counter_total_sum_generic` | 11 | 44 | 0.15 | 1.68 | 11.46× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 2.02 | 11.21× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.85 | 10.24× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 1.02 | 9.47× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.25 | 2.32 | 9.31× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 3.78 | 8.15× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.69 | 8.14× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.80 | 7.83× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.68 | 7.16× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.15 | 0.99 | 6.79× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.41 | 5.74× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.45 | 5.70× |
| `gauge_max_bare` | 1 | 4 | 0.08 | 0.45 | 5.57× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.40 | 5.42× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.46 | 5.34× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.41 | 4.99× |
| `gauge_sum_bare` | 2 | 8 | 0.11 | 0.49 | 4.53× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.37 | 4.53× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.09 | 0.38 | 4.32× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.41 | 2.61× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.19 | 1.85× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.24 | 1.73× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 0.12 | 1.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 0.12 | 0.87× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 0.11 | 0.75× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.23× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.73 | 14.25 | 19.42× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.21 | 3.82 | 18.02× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.45 | 7.43 | 16.69× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.30 | 3.82 | 12.76× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.98 | 11.64× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.47 | 5.48 | 11.60× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.49 | 5.61 | 11.37× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.18 | 10.85× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 1.03 | 11.19 | 10.82× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.77 | 8.22 | 10.72× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.28 | 2.94 | 10.54× |
| `counter_total_sum_generic` | 11 | 44 | 0.21 | 1.97 | 9.33× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.20 | 1.77 | 8.87× |
| `counter_ratio_generic` | 2 | 8 | 0.44 | 3.85 | 8.84× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.31 | 2.67 | 8.66× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.64 | 7.64× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.84 | 6.43 | 7.62× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.16 | 1.23 | 7.58× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.24 | 1.84 | 7.56× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.90 | 7.46× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.74 | 5.21 | 7.07× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.19 | 1.35 | 7.00× |
| `counter_rate_bare_generic` | 2 | 8 | 0.20 | 1.37 | 6.73× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.11 | 0.71 | 6.53× |
| `memory_util_pct` | 1 | 4 | 0.09 | 0.62 | 6.50× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.20 | 1.26 | 6.40× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.14 | 0.82 | 5.97× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.09 | 0.51 | 5.43× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.46 | 5.08× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.11 | 0.54 | 4.99× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.10 | 0.46 | 4.50× |
| `gauge_avg_scaled` | 6 | 24 | 0.09 | 0.41 | 4.47× |
| `gauge_sum_bare` | 2 | 8 | 0.14 | 0.55 | 4.03× |
| `gauge_max_bare` | 1 | 4 | 0.12 | 0.42 | 3.69× |
| `gauge_sum_scaled` | 1 | 4 | 0.12 | 0.41 | 3.35× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.23 | 3.17× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.20 | 0.41 | 2.03× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.14 | 0.24 | 1.70× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.18 | 0.25 | 1.39× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.14 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.14 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.55 | 13.88 | 25.33× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.54 | 10.85 | 19.97× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.23 | 4.28 | 18.41× |
| `counter_ratio_generic` | 2 | 8 | 0.26 | 3.85 | 14.65× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.20 | 2.80 | 14.25× |
| `counter_rate_bare_generic` | 2 | 8 | 0.07 | 1.01 | 13.78× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 0.97 | 13.26× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.75 | 9.79 | 13.12× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.27 | 3.42 | 12.72× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.52 | 6.56 | 12.57× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.40 | 4.98 | 12.38× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.40 | 4.94 | 12.29× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.25 | 3.03 | 11.93× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 1.07 | 11.92× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.20 | 2.39 | 11.87× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.73 | 11.57× |
| `counter_ratio_scaled` | 1 | 4 | 0.25 | 2.86 | 11.23× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.80 | 10.72× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.79 | 10.67× |
| `counter_total_sum_generic` | 11 | 44 | 0.16 | 1.71 | 10.52× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.73 | 9.69× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.60 | 9.42× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.14 | 1.33 | 9.29× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.17 | 1.52 | 9.11× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 1.29 | 8.73× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.97 | 8.57× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.56 | 4.74 | 8.44× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.77 | 7.37× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.63 | 7.22× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.36 | 5.62× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.34 | 5.40× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.37 | 5.26× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.08 | 0.40 | 5.14× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.37 | 4.88× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.35 | 4.80× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.33 | 4.63× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.10 | 0.41 | 4.25× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.37 | 4.06× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.20 | 0.45 | 2.18× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.19 | 1.84× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.20 | 1.73× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 21.13× | 3.32 | 70.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.69× | 0.37 | 6.14 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 16.10× | 0.07 | 1.15 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 16.01× | 0.12 | 1.88 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.78× | 0.11 | 1.76 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.77× | 0.10 | 1.52 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 14.30× | 0.13 | 1.83 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.88× | 0.10 | 1.39 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.78× | 0.14 | 1.87 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 12.91× | 0.14 | 1.77 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="used"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `gauge_ratio_with_labels_ignoring` | `gpu_pcie_throughput{direction="transmit"} / ignoring(direction) gpu_pcie_bandwidth` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 20.45× | 3.50 | 71.62 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.27× | 0.11 | 1.83 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.44× | 0.10 | 1.53 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 14.99× | 0.07 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.30× | 0.35 | 5.07 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.13× | 0.12 | 1.72 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.10× | 0.06 | 0.79 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 13.81× | 0.19 | 2.60 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 13.60× | 0.13 | 1.76 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 13.44× | 0.05 | 0.70 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_occupancy) / 100` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="free"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_tokens_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 22.01× | 3.28 | 72.25 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 15.33× | 0.13 | 2.03 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.68× | 0.20 | 3.00 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 13.90× | 0.45 | 6.21 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 13.69× | 0.13 | 1.80 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="tasklet"}[5m])) / 1000000000` |
| 13.58× | 0.16 | 2.14 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 13.54× | 0.13 | 1.73 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 13.53× | 0.05 | 0.74 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 13.27× | 0.20 | 2.68 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 13.10× | 0.12 | 1.61 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="timer"}[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 22.99× | 0.18 | 4.09 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 20.53× | 3.57 | 73.20 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.31× | 0.07 | 1.13 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 15.11× | 0.19 | 2.83 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 15.05× | 0.12 | 1.85 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.05× | 0.39 | 5.83 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.46× | 0.12 | 1.71 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 14.31× | 0.14 | 1.97 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 14.14× | 0.13 | 1.80 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.99× | 0.13 | 1.75 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 33.65× | 0.13 | 4.51 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 31.67× | 1.31 | 41.34 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 21.88× | 0.60 | 13.16 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 21.46× | 0.42 | 9.09 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.01× | 0.37 | 7.42 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 18.91× | 0.39 | 7.34 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.51× | 0.32 | 5.55 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 17.15× | 0.40 | 6.81 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 17.13× | 0.38 | 6.55 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 16.98× | 0.40 | 6.79 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.09 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.00× | 0.06 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.03× | 0.06 | 1.13 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 17.99× | 0.38 | 6.86 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 17.38× | 0.16 | 2.86 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 16.44× | 0.13 | 2.08 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 16.44× | 0.11 | 1.86 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.73× | 0.05 | 0.85 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 15.39× | 0.10 | 1.59 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 15.37× | 0.13 | 2.05 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 15.23× | 0.13 | 2.03 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 14.87× | 0.15 | 2.16 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 36.20× | 0.13 | 4.86 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 24.83× | 0.38 | 9.48 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 21.65× | 0.12 | 2.50 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 19.61× | 1.01 | 19.89 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 18.21× | 3.66 | 66.57 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 17.11× | 0.69 | 11.74 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.69× | 0.17 | 2.83 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 16.42× | 0.11 | 1.78 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 16.03× | 0.21 | 3.39 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 15.21× | 0.22 | 3.31 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.08 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.07 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_upstream_responses_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_tokens_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 53.27× | 0.36 | 19.43 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 41.22× | 0.25 | 10.29 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 22.58× | 0.86 | 19.53 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 21.86× | 0.23 | 4.97 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 19.33× | 0.26 | 5.10 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 19.29× | 0.44 | 8.43 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 18.97× | 0.29 | 5.48 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 18.24× | 0.35 | 6.31 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 18.04× | 0.30 | 5.42 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 14.81× | 0.32 | 4.76 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.11 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.00× | 0.16 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.12 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_tokens_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 32.06× | 1.41 | 45.05 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 26.26× | 0.49 | 12.92 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 21.15× | 0.22 | 4.64 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 19.84× | 0.14 | 2.69 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 19.60× | 0.19 | 3.78 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 18.62× | 0.19 | 3.52 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 18.59× | 0.39 | 7.27 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 18.21× | 0.20 | 3.72 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 18.01× | 0.45 | 8.12 | `rezolus_cpu_aperf_chain_per_id` | `sum by (id) (irate(cpu_tsc[5m])) * sum by (id) (irate(cpu_aperf[5m])) / sum by (id) (irate(cpu_mperf…` |
| 17.51× | 0.20 | 3.50 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.00× | 0.11 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="event"}` |
| 0.00× | 0.07 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 24.13× | 0.21 | 5.12 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 20.31× | 4.74 | 96.35 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 19.95× | 0.37 | 7.43 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{state="user",name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 19.51× | 0.82 | 16.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 19.42× | 0.73 | 14.25 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 18.01× | 0.24 | 4.27 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 17.55× | 0.23 | 3.99 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 16.12× | 0.21 | 3.45 | `counter_total_sum_generic` | `sum(irate(cpu_branch_instructions[5m]))` |
| 15.86× | 0.31 | 4.97 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 15.39× | 0.26 | 3.99 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_hits{source="cachecannon"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `get_latency{source="cachecannon"}` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_size{op="write"}` |
| 0.01× | 0.14 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.10 | 0.00 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 31.99× | 1.23 | 39.47 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 25.33× | 0.55 | 13.88 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 21.62× | 0.19 | 4.06 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 19.97× | 0.54 | 10.85 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 19.95× | 0.23 | 4.64 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 19.38× | 0.19 | 3.59 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 19.16× | 0.23 | 4.39 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 18.26× | 0.17 | 3.18 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 18.03× | 0.21 | 3.81 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 17.47× | 0.23 | 3.94 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.08 | 0.00 | `gauge_bare` | `scheduler_running` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.15 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.13 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |

