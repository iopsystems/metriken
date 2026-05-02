# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 309.2 ms | 20.8 ms | 7.2 ms | 471.0 ms | 1.6 ms | 479.9 ms |
| `AB_base_pin.parquet` | 3.1 | 302.3 ms | 8.5 ms | 2.3 ms | 489.6 ms | 0.1 ms | 507.3 ms |
| `AB_level.parquet` | 3.3 | 325.1 ms | 9.0 ms | 2.1 ms | 490.8 ms | 0.1 ms | 494.4 ms |
| `AB_level_pin.parquet` | 2.8 | 315.0 ms | 9.2 ms | 2.7 ms | 485.5 ms | 0.2 ms | 509.2 ms |
| `cachecannon.parquet` | 3.8 | 348.4 ms | 8.4 ms | 2.5 ms | 1048.6 ms | 0.2 ms | 1067.3 ms |
| `demo.parquet` | 1.2 | 107.3 ms | 10.3 ms | 2.8 ms | 153.8 ms | 0.2 ms | 158.4 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 727.0 ms | 9.1 ms | 2.4 ms | 5294.1 ms | 0.4 ms | 5155.4 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2595.5 ms | 28.9 ms | 3.1 ms | 16508.6 ms | 0.9 ms | 15013.9 ms |
| `sglang_gemma3.parquet` | 2.1 | 304.1 ms | 25.6 ms | 3.7 ms | 1311.2 ms | 0.6 ms | 1300.5 ms |
| `vllm.parquet` | 4.0 | 368.4 ms | 19.4 ms | 2.5 ms | 3604.1 ms | 0.6 ms | 3602.3 ms |
| `vllm_gemma3.parquet` | 2.1 | 343.6 ms | 18.4 ms | 3.3 ms | 1356.6 ms | 0.9 ms | 1317.5 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.111 | 0.708 | 6.38× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.113 | 0.703 | 6.24× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.110 | 0.690 | 6.29× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.113 | 0.716 | 6.32× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.157 | 1.086 | 6.90× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.081 | 0.510 | 6.30× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.179 | 1.124 | 6.28× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.873 | 3.043 | 3.49× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.125 | 0.888 | 7.08× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.187 | 1.290 | 6.91× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.146 | 1.172 | 8.01× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.45 | 14.78× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 2.63 | 14.21× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.68 | 12.72× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.69 | 12.58× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 2.00 | 12.15× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.61 | 12.05× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.49 | 11.93× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.54 | 11.37× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.35 | 11.32× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.02 | 11.18× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 1.12 | 11.11× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.74 | 11.09× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.66 | 10.40× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.73 | 10.36× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.69 | 10.03× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.27 | 9.88× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.06 | 9.72× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.98 | 9.68× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.40 | 9.06× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.34 | 3.07 | 8.91× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.20 | 1.64 | 8.35× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 2.74 | 7.07× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.29 | 5.66× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.40 | 3.46× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.07 | 0.23 | 3.27× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.27 | 2.64× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.22 | 2.62× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.01 | 0.24× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.24× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.01 | 0.15× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.01 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.64 | 17.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.73 | 12.97× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.68 | 12.76× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 2.20 | 12.72× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.73 | 12.53× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.47 | 12.26× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.38 | 11.97× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.71 | 11.73× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.98 | 11.38× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 2.03 | 11.23× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.59 | 10.98× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 1.10 | 10.76× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.72 | 9.99× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.69 | 9.61× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 1.04 | 9.39× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.26 | 9.11× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.33 | 2.86 | 8.75× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.79 | 8.53× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.99 | 8.15× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.37 | 8.09× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 1.71 | 7.91× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.36 | 2.67 | 7.50× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.27 | 6.23× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.44 | 3.53× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.23 | 2.87× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.23 | 2.82× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.22 | 2.51× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.22× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.01 | 0.22× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.22× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.14× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.10 | 0.01 | 0.14× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.60 | 15.56× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 2.20 | 13.21× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.69 | 12.97× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 1.04 | 12.92× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 2.09 | 12.09× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.71 | 11.99× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 2.38 | 11.63× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.49 | 11.44× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.14 | 1.55 | 10.92× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.94 | 10.82× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.38 | 10.65× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.80 | 10.62× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 2.54 | 10.53× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.68 | 9.86× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.08 | 9.72× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.41 | 9.56× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.33 | 3.13 | 9.41× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.29 | 9.21× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.69 | 8.92× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.30 | 7.56× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.00 | 7.16× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.71 | 7.08× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.37 | 2.52 | 6.81× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.42 | 3.59× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.09 | 0.27 | 2.87× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.21 | 2.41× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.21 | 1.66× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.01 | 0.24× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.24× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.23× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.01 | 0.12× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.58 | 16.44× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.70 | 13.25× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 2.28 | 13.12× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.36 | 12.74× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.36 | 12.16× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.53 | 11.84× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.55 | 11.80× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.96 | 11.57× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.72 | 11.46× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 2.68 | 11.42× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 2.04 | 11.38× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 1.05 | 10.94× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.74 | 10.51× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.25 | 9.45× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.69 | 9.39× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.02 | 9.36× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 1.58 | 8.91× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.06 | 8.72× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 2.99 | 8.53× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.40 | 8.17× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.73 | 8.13× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.37 | 2.70 | 7.29× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.30 | 6.32× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.41 | 3.23× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.29 | 3.00× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.25 | 2.65× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.22 | 2.56× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.01 | 0.23× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.22× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.01 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.14× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 9.03 | 23.61× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 2.63 | 15.14× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.25 | 3.11 | 12.69× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.32 | 4.03 | 12.62× |
| `counter_total_sum_generic` | 11 | 44 | 0.13 | 1.40 | 11.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.22 | 2.33 | 10.58× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.39 | 4.08 | 10.50× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.79 | 10.16× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.75 | 7.30 | 9.78× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.83 | 9.70× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.76 | 7.19 | 9.40× |
| `counter_ratio_scaled` | 1 | 4 | 0.53 | 4.89 | 9.18× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.32 | 2.88 | 8.95× |
| `counter_ratio_generic` | 2 | 8 | 0.54 | 4.71 | 8.80× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.32 | 11.45 | 8.67× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.16 | 8.67× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.13 | 1.06 | 8.44× |
| `counter_irate_total_scaled` | 1 | 4 | 0.10 | 0.83 | 8.41× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.76 | 7.59× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.41 | 6.73× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 0.83 | 6.64× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.36 | 5.16× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.40 | 4.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.25 | 2.80× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.46 | 2.80× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.22 | 2.65× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.12 | 0.23 | 1.93× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.18× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.15× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.21 | 0.01 | 0.07× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.31 | 0.02 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.17 | 2.81 | 16.44× |
| `counter_ratio_generic` | 2 | 8 | 0.14 | 2.25 | 16.13× |
| `counter_irate_total_scaled` | 1 | 4 | 0.05 | 0.84 | 15.63× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.57 | 13.24× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.65 | 12.72× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.56 | 11.97× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.36 | 4.25 | 11.63× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.74 | 11.59× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.53 | 11.16× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 0.75 | 10.87× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.23 | 10.84× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.54 | 10.41× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.42 | 10.34× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.82 | 10.19× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.42 | 9.94× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 3.10 | 8.12× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.88 | 7.94× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.06 | 0.41 | 6.73× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.16 | 4.34× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.45 | 3.96× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.23 | 3.50× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.22 | 2.67× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.24 | 2.23× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.01 | 0.35× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.01 | 0.34× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.18× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.23 | 7.85 | 34.63× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.21 | 2.26 | 10.96× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.45 | 10.60× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 1.74 | 10.50× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.78 | 10.40× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.12 | 1.22 | 10.25× |
| `counter_irate_total_scaled` | 1 | 4 | 0.10 | 1.00 | 10.15× |
| `counter_ratio_scaled` | 1 | 4 | 0.35 | 3.52 | 10.07× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.91 | 8.87 | 9.79× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.11 | 1.10 | 9.69× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.24 | 2.19 | 9.18× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.25 | 2.31 | 9.16× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.10 | 0.88 | 9.05× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.24 | 2.13 | 8.90× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.86 | 8.77× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.15 | 1.30 | 8.62× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.26 | 2.25 | 8.61× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.22 | 1.86 | 8.51× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.46 | 8.15× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.48 | 3.83 | 8.03× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.74 | 7.90× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.11 | 0.84 | 7.82× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.36 | 2.46 | 6.84× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.45 | 6.36× |
| `counter_rate_bare_generic` | 2 | 8 | 0.21 | 1.24 | 5.79× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.50 | 5.71× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.45 | 4.79× |
| `gauge_bare` | 10 | 40 | 0.08 | 0.37 | 4.50× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.39 | 4.44× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.40 | 4.42× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.25 | 2.47× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.22 | 2.02× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.11 | 0.02 | 0.19× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.18× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.15 | 0.02 | 0.16× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.13 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.04× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_generic` | 2 | 8 | 0.11 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.06 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.18 | 0.00 | 0.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.13 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns` | 1 | 4 | 0.37 | 14.16 | 38.76× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.33 | 9.50 | 29.08× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.67 | 17.52 | 26.10× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.27 | 4.57 | 16.64× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.40 | 4.48 | 11.29× |
| `memory_util_pct` | 1 | 4 | 0.17 | 1.46 | 8.73× |
| `counter_irate_total_scaled` | 1 | 4 | 0.20 | 1.59 | 8.13× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.97 | 7.72 | 7.96× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.95 | 7.16 | 7.57× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.19 | 1.40 | 7.49× |
| `gauge_subtract` | 1 | 4 | 0.18 | 1.31 | 7.24× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.30 | 2.13 | 7.13× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.18 | 1.24 | 7.08× |
| `gauge_bare` | 10 | 40 | 0.11 | 0.73 | 6.53× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.16 | 1.04 | 6.51× |
| `counter_total_sum_generic` | 11 | 44 | 0.32 | 2.09 | 6.49× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.72 | 4.68 | 6.45× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.69 | 4.42 | 6.43× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.99 | 6.31 | 6.37× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.21 | 1.34 | 6.27× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.10 | 0.60 | 6.19× |
| `counter_ratio_generic` | 2 | 8 | 0.75 | 4.48 | 5.97× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.28 | 7.32 | 5.74× |
| `counter_ratio_scaled` | 1 | 4 | 0.76 | 4.35 | 5.69× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 3.80 | 21.18 | 5.58× |
| `counter_irate_total_mul` | 2 | 8 | 0.22 | 1.19 | 5.44× |
| `gauge_sum_scaled` | 1 | 4 | 0.10 | 0.55 | 5.23× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.72 | 14.03 | 5.17× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.12 | 0.60 | 4.94× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.68 | 3.33 | 4.92× |
| `gauge_sum_with_labels` | 5 | 20 | 0.11 | 0.51 | 4.61× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.01 | 21.53 | 4.30× |
| `gauge_max_bare` | 1 | 4 | 0.14 | 0.55 | 3.99× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.42 | 1.56 | 3.74× |
| `gauge_sum_bare` | 2 | 8 | 0.15 | 0.51 | 3.47× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.17 | 7.51 | 3.46× |
| `counter_rate_bare_generic` | 2 | 8 | 0.62 | 1.97 | 3.16× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.28 | 0.49 | 1.78× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.23 | 1.65× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.15 | 0.22 | 1.47× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.00 | 0.04× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.08 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.10 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.06 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.18 | 2.44 | 13.87× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 2.41 | 13.34× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.85 | 13.24× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.51 | 6.43 | 12.70× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.30 | 3.81 | 12.59× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.17 | 2.12 | 12.23× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.18 | 2.18 | 12.06× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.17 | 2.06 | 12.02× |
| `counter_ratio_generic` | 2 | 8 | 0.22 | 2.64 | 11.85× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.31 | 3.53 | 11.43× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 0.86 | 11.37× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.69 | 10.68× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.68 | 7.25 | 10.59× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.45 | 4.48 | 10.03× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.57 | 10.00× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.35 | 9.80× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.84 | 9.43× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.81 | 9.40× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.44 | 9.19× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.64 | 9.00× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.54 | 8.82× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.62 | 8.64× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.66 | 8.32× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.16 | 1.31 | 8.22× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.49 | 3.76 | 7.61× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 3.12 | 7.26× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.38 | 6.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.37 | 5.54× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.36 | 5.52× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.33 | 5.37× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.34 | 5.19× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.32 | 5.06× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.31 | 4.81× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.32 | 4.61× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.24 | 4.15× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.08 | 0.23 | 2.76× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.09 | 0.22 | 2.44× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.21 | 2.25× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.14 | 0.31 | 2.15× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.18 | 2.12× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.17 | 1.92× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.18× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.87 | 12.95× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.24 | 12.18× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.40 | 4.83 | 12.16× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.24 | 2.91 | 11.99× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.64 | 11.86× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.55 | 6.49 | 11.83× |
| `counter_ratio_generic` | 2 | 8 | 0.32 | 3.55 | 11.11× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.48 | 5.08 | 10.62× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.94 | 9.93 | 10.60× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.77 | 8.09 | 10.49× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.66 | 10.29× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.25 | 2.55 | 10.25× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.27 | 2.66 | 10.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 1.31 | 10.00× |
| `counter_total_sum_generic` | 11 | 44 | 0.18 | 1.75 | 9.82× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.99 | 8.82× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.20 | 1.79 | 8.77× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.88 | 8.68× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.15 | 1.30 | 8.56× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 1.19 | 8.34× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.68 | 5.46 | 8.06× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.13 | 1.04 | 8.05× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.73 | 7.98× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.39 | 2.91 | 7.50× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.53 | 7.19× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.72 | 4.87 | 6.73× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.12 | 0.71 | 5.95× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.46 | 5.93× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.45 | 5.68× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.40 | 5.47× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.41 | 4.79× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.36 | 4.77× |
| `gauge_avg_scaled` | 6 | 24 | 0.09 | 0.40 | 4.59× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.38 | 4.52× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.12 | 0.43 | 3.70× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.21 | 3.37× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.21 | 0.46 | 2.21× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.23 | 1.99× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.20 | 1.93× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.20× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.07 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.11 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.46 | 8.22 | 17.82× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.51 | 8.44 | 16.56× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.33 | 5.08 | 15.62× |
| `counter_ratio_scaled` | 1 | 4 | 0.22 | 2.99 | 13.61× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.20 | 2.69 | 13.56× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.20 | 2.68 | 13.39× |
| `counter_ratio_generic` | 2 | 8 | 0.27 | 3.67 | 13.38× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 3.15 | 13.32× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.75 | 9.78 | 13.09× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.09 | 1.10 | 12.87× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.74 | 12.66× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.94 | 12.24× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 1.08 | 12.14× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.37 | 4.34 | 11.77× |
| `counter_total_sum_generic` | 11 | 44 | 0.16 | 1.90 | 11.64× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.23 | 2.60 | 11.43× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.08 | 10.69× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.71 | 10.60× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.27 | 10.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 1.28 | 9.98× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.65 | 9.37× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.52 | 4.65 | 8.92× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.20 | 1.79 | 8.76× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.23 | 8.69× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.77 | 8.45× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 1.13 | 8.31× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.52 | 4.13 | 7.93× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.86 | 7.47× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.11 | 0.76 | 6.84× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.45 | 6.78× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.49 | 5.97× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.40 | 5.58× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.38 | 5.44× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.10 | 0.46 | 4.57× |
| `gauge_max_bare` | 1 | 4 | 0.10 | 0.43 | 4.44× |
| `gauge_avg_scaled` | 6 | 24 | 0.10 | 0.42 | 4.31× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.29 | 3.93× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.11 | 0.37 | 3.40× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.43 | 2.87× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.23 | 2.33× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.22 | 2.08× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.04× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.80× | 0.10 | 1.55 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.31× | 0.07 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.66× | 0.11 | 1.58 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.21× | 0.18 | 2.63 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 13.63× | 0.12 | 1.70 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.23× | 0.05 | 0.72 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 12.79× | 0.13 | 1.60 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="tasklet"}[5m]))` |
| 12.72× | 0.05 | 0.68 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.58× | 0.13 | 1.69 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 12.48× | 0.10 | 1.30 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.27× | 0.05 | 0.78 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 17.16× | 0.10 | 1.65 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.72× | 0.11 | 1.70 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.63× | 0.06 | 0.98 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.00× | 0.12 | 1.68 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 12.97× | 0.21 | 2.73 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 12.92× | 0.16 | 2.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 12.86× | 0.13 | 1.70 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 12.77× | 0.10 | 1.23 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 12.76× | 0.05 | 0.68 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.90× | 0.10 | 1.73 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.74× | 0.12 | 1.70 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.46× | 0.07 | 0.94 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.36× | 0.09 | 1.28 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 14.27× | 0.05 | 0.71 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 13.21× | 0.17 | 2.20 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 12.97× | 0.05 | 0.69 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.92× | 0.08 | 1.04 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 12.69× | 0.13 | 1.60 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 12.35× | 0.17 | 2.12 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-prefill"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.69× | 0.10 | 1.70 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.49× | 0.05 | 0.72 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 14.76× | 0.06 | 0.96 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.48× | 0.10 | 1.38 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 14.38× | 0.10 | 1.44 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.36× | 0.12 | 1.78 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 13.99× | 0.12 | 1.62 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.58× | 0.12 | 1.63 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 13.25× | 0.05 | 0.70 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 13.12× | 0.17 | 2.28 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-prefill"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 29.73× | 0.14 | 4.22 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 23.61× | 0.38 | 9.03 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 15.38× | 0.17 | 2.64 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 15.00× | 0.18 | 2.75 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 14.99× | 0.09 | 1.40 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.70× | 0.19 | 2.67 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 13.69× | 0.17 | 2.28 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 13.47× | 0.21 | 2.83 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 13.07× | 0.24 | 3.11 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 12.91× | 0.20 | 2.63 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.10 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.10 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.05 | 0.00 | `gauge_avg_scaled` | `avg(gpu_memory_utilization) / 100` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.12 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.95× | 0.10 | 1.77 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 16.44× | 0.17 | 2.81 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 16.13× | 0.14 | 2.25 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 15.63× | 0.05 | 0.84 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 15.27× | 0.06 | 0.96 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.95× | 0.13 | 1.80 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 13.89× | 0.09 | 1.29 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 13.54× | 0.12 | 1.61 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.41× | 0.13 | 1.73 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 13.11× | 0.14 | 1.80 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_memory_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_http_request_duration_seconds{source="sglang-router"}` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 34.63× | 0.23 | 7.85 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 19.56× | 0.14 | 2.80 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 13.70× | 0.33 | 4.54 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_migrations{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 13.62× | 0.13 | 1.83 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 13.54× | 0.16 | 2.13 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 12.96× | 0.15 | 1.99 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 12.68× | 0.13 | 1.63 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 12.57× | 0.07 | 0.88 | `gauge_ratio_with_labels_ignoring` | `gpu_pcie_throughput{direction="receive"} / ignoring(direction) gpu_pcie_bandwidth` |
| 11.34× | 0.22 | 2.49 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 11.27× | 0.17 | 1.90 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.06 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.09 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.16 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.10 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_occupancy) / 100` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 38.76× | 0.37 | 14.16 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 29.08× | 0.33 | 9.50 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 26.18× | 0.19 | 5.00 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 26.10× | 0.67 | 17.52 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 22.80× | 0.34 | 7.85 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 18.51× | 0.27 | 5.09 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 18.17× | 0.25 | 4.57 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 16.64× | 0.29 | 4.76 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 16.33× | 0.29 | 4.67 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 15.41× | 0.31 | 4.85 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_hits{source="cachecannon"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_tokens_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.14 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_dram_bandwidth_utilization) / 100` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.10 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_misses{source="cachecannon"}[5s]))` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.22× | 0.16 | 2.29 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.87× | 0.18 | 2.44 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 13.55× | 0.17 | 2.32 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 13.34× | 0.18 | 2.41 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 13.24× | 0.06 | 0.85 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 12.96× | 0.15 | 2.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 12.70× | 0.51 | 6.43 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 12.69× | 0.17 | 2.11 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 12.60× | 0.21 | 2.64 | `counter_ratio_generic` | `sum(irate(cpu_branch_misses[5m])) / sum(irate(cpu_branch_instructions[5m]))` |
| 12.59× | 0.30 | 3.81 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.09× | 0.20 | 2.77 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.88× | 0.42 | 5.77 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="read",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 13.05× | 0.27 | 3.55 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 12.95× | 0.07 | 0.87 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 12.56× | 0.35 | 4.37 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 12.54× | 0.21 | 2.66 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 12.27× | 0.13 | 1.64 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 12.20× | 0.21 | 2.54 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 12.18× | 0.10 | 1.24 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 12.16× | 0.40 | 4.83 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `get_latency{source="cachecannon"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-prefill"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.82× | 0.46 | 8.22 | `rezolus_cpu_aperf_chain_per_id` | `sum by (id) (irate(cpu_tsc[5m])) * sum by (id) (irate(cpu_aperf[5m])) / sum by (id) (irate(cpu_mperf…` |
| 16.56× | 0.51 | 8.44 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 16.48× | 0.17 | 2.79 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 16.02× | 0.18 | 2.82 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 15.62× | 0.33 | 5.08 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 15.19× | 0.17 | 2.55 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 14.08× | 0.20 | 2.75 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 13.97× | 0.18 | 2.56 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.86× | 0.18 | 2.45 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.61× | 0.22 | 2.99 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.00× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.00× | 0.10 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `blockio_size{op="write"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `set_latency{source="cachecannon"}` |

