# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 320.2 ms | 19.2 ms | 7.4 ms | 233.6 ms | 1.2 ms | 249.4 ms |
| `AB_base_pin.parquet` | 3.1 | 316.0 ms | 9.2 ms | 2.8 ms | 228.1 ms | 0.1 ms | 236.6 ms |
| `AB_level.parquet` | 3.3 | 335.1 ms | 10.7 ms | 2.9 ms | 219.7 ms | 0.2 ms | 234.9 ms |
| `AB_level_pin.parquet` | 2.8 | 321.7 ms | 11.2 ms | 3.1 ms | 226.6 ms | 0.2 ms | 243.7 ms |
| `cachecannon.parquet` | 3.8 | 349.1 ms | 10.0 ms | 2.9 ms | 244.2 ms | 0.2 ms | 255.5 ms |
| `demo.parquet` | 1.2 | 105.8 ms | 9.3 ms | 2.8 ms | 106.5 ms | 0.2 ms | 113.1 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 705.0 ms | 8.8 ms | 2.3 ms | 957.0 ms | 0.4 ms | 1028.4 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2856.1 ms | 20.8 ms | 3.1 ms | 3439.5 ms | 0.7 ms | 3352.9 ms |
| `sglang_gemma3.parquet` | 2.1 | 313.6 ms | 29.4 ms | 3.7 ms | 736.2 ms | 0.8 ms | 650.9 ms |
| `vllm.parquet` | 4.0 | 394.4 ms | 24.2 ms | 3.4 ms | 481.5 ms | 0.9 ms | 511.4 ms |
| `vllm_gemma3.parquet` | 2.1 | 330.2 ms | 19.1 ms | 3.1 ms | 452.4 ms | 0.7 ms | 486.3 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.115 | 0.728 | 6.30× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.117 | 0.714 | 6.09× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.104 | 0.587 | 5.62× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.106 | 0.608 | 5.71× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.165 | 1.075 | 6.53× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.072 | 0.445 | 6.22× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.163 | 0.979 | 6.02× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.937 | 3.022 | 3.22× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.141 | 0.996 | 7.08× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.198 | 1.197 | 6.05× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.139 | 1.067 | 7.67× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.58 | 15.06× |
| `counter_ratio_generic` | 2 | 8 | 0.19 | 2.64 | 14.03× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.87 | 13.38× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.71 | 12.55× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.49 | 12.18× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.68 | 11.90× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.49 | 11.84× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.39 | 11.78× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.61 | 11.37× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.51 | 11.12× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 1.06 | 10.55× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.95 | 10.45× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.58 | 10.18× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.67 | 9.96× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.76 | 9.77× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 1.09 | 9.42× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.91 | 8.85× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.30 | 8.70× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.36 | 3.04 | 8.45× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.37 | 8.44× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.20 | 1.67 | 8.17× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 2.57 | 5.94× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.24 | 5.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.07 | 0.24 | 3.39× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.26 | 2.64× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.22 | 2.62× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.38 | 2.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.29× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.25× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.22× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.04× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.65 | 14.90× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.84 | 13.58× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.71 | 12.83× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 2.20 | 12.77× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.46 | 12.73× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.10 | 12.60× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.64 | 12.36× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 2.81 | 12.26× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.46 | 11.95× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.49 | 11.59× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.69 | 11.02× |
| `counter_ratio_generic` | 2 | 8 | 0.19 | 2.14 | 10.98× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 1.11 | 10.97× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.11 | 9.81× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.40 | 9.66× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.69 | 9.24× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.24 | 8.46× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.60 | 8.22× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.36 | 2.92 | 8.13× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.75 | 7.18× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 2.64 | 6.80× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 0.97 | 6.68× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.28 | 6.34× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.43 | 3.51× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.24 | 3.31× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.24 | 2.75× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.23 | 2.59× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.03 | 0.36× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.23× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.21× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.21× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.11 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.91 | 12.26× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.61 | 11.64× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.11 | 1.33 | 11.61× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.22 | 11.33× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.70 | 11.23× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.19 | 2.13 | 11.15× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.21 | 2.31 | 11.06× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.27 | 10.78× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.54 | 10.17× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.85 | 10.12× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 1.74 | 9.75× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.90 | 9.69× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.23 | 9.62× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.62 | 9.49× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.78 | 8.86× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.89 | 8.68× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.09 | 8.49× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.34 | 8.28× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.32 | 2.51 | 7.85× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.58 | 7.69× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.16 | 1.22 | 7.63× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.37 | 2.10 | 5.64× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.25 | 5.57× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.19 | 2.89× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.30 | 2.55× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.20 | 2.46× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.16 | 1.98× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.30× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.25× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.02× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.35 | 13.36× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.11 | 1.38 | 12.68× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.10 | 1.11 | 11.41× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.57 | 11.20× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.21 | 11.18× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.19 | 2.10 | 11.05× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.57 | 11.05× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.65 | 10.75× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 1.80 | 10.56× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.20 | 2.13 | 10.40× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.62 | 10.35× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.61 | 9.72× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.90 | 9.36× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.67 | 9.35× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.87 | 9.22× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.88 | 9.17× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 0.83 | 8.63× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 1.03 | 8.42× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.33 | 2.53 | 7.59× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.36 | 7.26× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.17 | 1.24 | 7.10× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.24 | 6.07× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.37 | 2.13 | 5.70× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.20 | 2.86× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.29 | 2.73× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.18 | 2.24× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.16 | 1.94× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.03 | 0.37× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.25× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.24× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 8.73 | 22.98× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.19 | 2.62 | 13.66× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.32 | 4.03 | 12.77× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.27 | 2.85 | 10.70× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.22 | 2.31 | 10.52× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.44 | 10.49× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.41 | 4.20 | 10.34× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.76 | 10.26× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.90 | 9.59× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.81 | 9.31× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.72 | 6.56 | 9.07× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.72 | 6.50 | 8.98× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.30 | 11.61 | 8.96× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.31 | 2.73 | 8.78× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.13 | 8.42× |
| `counter_ratio_generic` | 2 | 8 | 0.57 | 4.80 | 8.35× |
| `counter_ratio_scaled` | 1 | 4 | 0.55 | 4.46 | 8.15× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.11 | 0.80 | 7.27× |
| `counter_irate_total_mul` | 2 | 8 | 0.15 | 0.95 | 6.58× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.14 | 0.91 | 6.48× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.35 | 5.69× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.44 | 5.56× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.41 | 3.44× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.23 | 2.25× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.24 | 2.04× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.26 | 1.99× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.21 | 0.37 | 1.76× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.10 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.11 | 0.02 | 0.16× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.19 | 0.02 | 0.11× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.29 | 0.02 | 0.07× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_scaled` | 1 | 4 | 0.05 | 0.79 | 15.69× |
| `counter_ratio_generic` | 2 | 8 | 0.14 | 2.05 | 14.89× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.11 | 1.57 | 13.81× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.18 | 2.40 | 13.48× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.46 | 12.67× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.74 | 12.31× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.31 | 11.87× |
| `counter_total_sum_generic` | 11 | 44 | 0.06 | 0.67 | 11.02× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.11 | 1.17 | 10.50× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 1.00 | 10.09× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.37 | 9.37× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 3.20 | 9.21× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.46 | 8.81× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.09 | 0.76 | 8.60× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.44 | 8.19× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.81 | 7.13× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 2.73 | 6.92× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.05 | 0.25 | 4.62× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.17 | 4.22× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.05 | 0.22 | 4.05× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.30 | 3.02× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.16 | 2.38× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.18 | 2.19× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.43× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.40× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.30× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.01 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.11× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.22 | 5.11 | 22.92× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 1.49 | 12.81× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.91 | 11.59× |
| `counter_total_sum_generic` | 11 | 44 | 0.13 | 1.23 | 9.37× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.05 | 9.21× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.85 | 9.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.10 | 0.92 | 9.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.77 | 8.92× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.41 | 3.52 | 8.67× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.23 | 1.95 | 8.65× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.75 | 8.39× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.65 | 8.37× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.24 | 1.92 | 8.15× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.23 | 1.89 | 8.07× |
| `counter_ratio_scaled` | 1 | 4 | 0.32 | 2.53 | 7.99× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.25 | 1.99 | 7.87× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.98 | 7.60 | 7.76× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.20 | 1.51 | 7.71× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.41 | 7.65× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.63 | 7.50× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.28 | 2.07 | 7.48× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.14 | 1.07 | 7.45× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.33 | 2.26 | 6.76× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.06 | 6.32× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.41 | 4.61× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.33 | 4.60× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.32 | 4.58× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.35 | 4.16× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.34 | 4.10× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.33 | 3.80× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.19 | 1.97× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.18 | 1.85× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.03 | 0.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.24× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.06 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.10 | 0.00 | 0.02× |
| `counter_ratio_generic` | 2 | 8 | 0.11 | 0.00 | 0.02× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns` | 1 | 4 | 0.39 | 13.57 | 34.83× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.29 | 8.09 | 28.14× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.78 | 16.97 | 21.83× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.26 | 4.43 | 17.31× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.45 | 4.71 | 10.55× |
| `memory_util_pct` | 1 | 4 | 0.17 | 1.57 | 9.08× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.16 | 1.42 | 8.96× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.15 | 1.21 | 8.31× |
| `gauge_subtract` | 1 | 4 | 0.15 | 1.23 | 8.25× |
| `counter_irate_total_scaled` | 1 | 4 | 0.21 | 1.64 | 7.68× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.05 | 7.31 | 6.98× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.02 | 6.91 | 6.77× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.19 | 1.25 | 6.74× |
| `gauge_bare` | 10 | 40 | 0.11 | 0.72 | 6.36× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.77 | 4.77 | 6.16× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 1.08 | 6.61 | 6.13× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.75 | 4.54 | 6.08× |
| `counter_total_sum_generic` | 11 | 44 | 0.33 | 2.00 | 5.97× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.17 | 1.01 | 5.80× |
| `counter_irate_total_mul` | 2 | 8 | 0.19 | 1.11 | 5.77× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.11 | 0.63 | 5.73× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.30 | 1.65 | 5.50× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 4.19 | 22.25 | 5.32× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.64 | 3.42 | 5.31× |
| `counter_ratio_generic` | 2 | 8 | 0.82 | 4.32 | 5.28× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.38 | 7.20 | 5.23× |
| `counter_ratio_scaled` | 1 | 4 | 0.80 | 4.03 | 5.02× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.98 | 14.11 | 4.74× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.12 | 0.53 | 4.54× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.32 | 21.55 | 4.05× |
| `gauge_sum_with_labels` | 5 | 20 | 0.12 | 0.46 | 3.91× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.43 | 3.84× |
| `gauge_sum_scaled` | 1 | 4 | 0.12 | 0.42 | 3.55× |
| `gauge_sum_bare` | 2 | 8 | 0.14 | 0.46 | 3.37× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.20 | 7.17 | 3.25× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.45 | 1.38 | 3.10× |
| `counter_rate_bare_generic` | 2 | 8 | 0.65 | 1.98 | 3.05× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.28 | 0.41 | 1.47× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.16 | 0.22 | 1.39× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.21 | 1.35× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.03 | 0.25× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.09 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.21 | 2.87 | 13.89× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.17 | 2.37 | 13.65× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.35 | 4.74 | 13.48× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.54 | 7.22 | 13.41× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 0.93 | 13.20× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 2.97 | 13.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.70 | 8.94 | 12.87× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.46 | 5.88 | 12.83× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.20 | 2.57 | 12.74× |
| `counter_ratio_generic` | 2 | 8 | 0.25 | 3.11 | 12.59× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.41 | 12.32× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.34 | 4.00 | 11.60× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 0.97 | 11.21× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.54 | 10.73× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 1.80 | 10.65× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.62 | 10.40× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.79 | 9.95× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.62 | 9.20× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.87 | 9.16× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.90 | 8.79× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.49 | 4.24 | 8.62× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.67 | 8.35× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.74 | 8.23× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.62 | 8.15× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 1.42 | 7.92× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.51 | 4.03 | 7.84× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.38 | 5.60× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.39 | 5.44× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.39 | 5.39× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.35 | 4.83× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.32 | 4.65× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.33 | 4.43× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.28 | 4.39× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.34 | 4.23× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.34 | 4.07× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.08 | 0.25 | 2.96× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.23 | 2.60× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.34 | 2.18× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 0.22 | 1.96× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.20 | 1.86× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.19 | 1.80× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.01 | 0.08× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 2.95 | 13.34× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.22 | 2.51 | 11.24× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.19 | 10.58× |
| `counter_ratio_generic` | 2 | 8 | 0.32 | 3.36 | 10.48× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.79 | 8.08 | 10.20× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.27 | 2.63 | 9.80× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.63 | 6.13 | 9.69× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.44 | 4.11 | 9.42× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 1.00 | 9.24 | 9.20× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.15 | 1.35 | 9.19× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.52 | 4.51 | 8.76× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.75 | 8.28× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 1.24 | 7.99× |
| `counter_total_sum_generic` | 11 | 44 | 0.20 | 1.55 | 7.94× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.71 | 7.80× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.62 | 7.50× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.56 | 7.43× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.78 | 7.42× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.14 | 0.98 | 7.24× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.55 | 7.22× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.24 | 1.72 | 7.17× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 1.01 | 7.06× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.17 | 1.19 | 7.02× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.85 | 6.85× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.40 | 2.77 | 6.84× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.77 | 5.21 | 6.81× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.72 | 4.39 | 6.06× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.08 | 0.42 | 5.56× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.42 | 5.30× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.42 | 4.85× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.37 | 4.81× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.35 | 4.50× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.39 | 4.46× |
| `gauge_max_bare` | 1 | 4 | 0.08 | 0.36 | 4.42× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.36 | 4.26× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.20 | 2.95× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.39 | 2.41× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.20 | 1.82× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.19 | 1.75× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.10 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.14 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.25 | 4.50 | 18.27× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.21 | 2.82 | 13.71× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.44 | 5.80 | 13.09× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.68 | 8.88 | 13.08× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.19 | 2.46 | 13.06× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.32 | 4.13 | 13.04× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 0.87 | 12.63× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 2.49 | 12.50× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.55 | 6.89 | 12.46× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.19 | 2.32 | 12.17× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.68 | 11.79× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 2.49 | 11.70× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.80 | 11.07× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.36 | 3.93 | 10.79× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 0.89 | 10.54× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 1.00 | 10.52× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 1.39 | 9.84× |
| `counter_total_sum_generic` | 11 | 44 | 0.15 | 1.44 | 9.81× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.93 | 9.21× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.62 | 9.20× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.72 | 9.10× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.68 | 9.09× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.14 | 1.30 | 9.08× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 1.58 | 8.92× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.53 | 4.68 | 8.76× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.22 | 8.72× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.57 | 8.44× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.72 | 8.26× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.49 | 3.54 | 7.17× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.45 | 6.26× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.40 | 5.69× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.08 | 0.40 | 5.16× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.38 | 5.14× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.38 | 5.14× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.36 | 5.05× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.35 | 5.05× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.38 | 4.82× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.27 | 4.24× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.36 | 2.32× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.21 | 2.12× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.88× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.03 | 0.36× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.76× | 0.11 | 1.66 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.55× | 0.12 | 1.83 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.30× | 0.05 | 0.65 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 14.03× | 0.19 | 2.64 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 13.75× | 0.12 | 1.68 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.38× | 0.07 | 0.87 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 13.05× | 0.07 | 0.87 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.76× | 0.14 | 1.79 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 12.55× | 0.06 | 0.71 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.20× | 0.07 | 0.80 | `counter_irate_sum_with_labels` | `sum(irate(cache_misses{source="cachecannon"}[5s]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.11× | 0.07 | 1.10 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.95× | 0.11 | 1.65 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.26× | 0.11 | 1.55 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.61× | 0.12 | 1.65 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 13.58× | 0.06 | 0.84 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 12.86× | 0.17 | 2.15 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 12.83× | 0.06 | 0.71 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.77× | 0.17 | 2.20 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 12.73× | 0.11 | 1.46 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="tasklet"}[5m])) / 1000000000` |
| 12.36× | 0.13 | 1.64 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.09 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.51× | 0.11 | 1.64 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 15.16× | 0.04 | 0.66 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 12.99× | 0.11 | 1.40 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 12.54× | 0.06 | 0.78 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.26× | 0.16 | 1.91 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 11.98× | 0.11 | 1.27 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 11.64× | 0.05 | 0.61 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 11.61× | 0.11 | 1.33 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 11.53× | 0.12 | 1.35 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 11.52× | 0.09 | 1.05 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.28× | 0.13 | 2.05 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 15.15× | 0.04 | 0.65 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 14.66× | 0.11 | 1.63 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 14.07× | 0.10 | 1.43 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 13.27× | 0.06 | 0.83 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.02× | 0.10 | 1.27 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.68× | 0.11 | 1.38 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 11.91× | 0.10 | 1.20 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 11.63× | 0.06 | 0.65 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 11.41× | 0.10 | 1.11 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="tasklet"}[5m])) / 1000000000` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 29.21× | 0.14 | 4.02 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 22.98× | 0.38 | 8.73 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 15.84× | 0.17 | 2.62 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 15.38× | 0.19 | 2.95 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 14.75× | 0.19 | 2.83 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 14.43× | 0.10 | 1.44 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.15× | 0.19 | 2.75 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 13.65× | 0.16 | 2.16 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 13.33× | 0.21 | 2.74 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 13.30× | 0.20 | 2.67 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.08 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.06 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.69× | 0.05 | 0.79 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 14.89× | 0.14 | 2.05 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 14.45× | 0.12 | 1.68 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 14.32× | 0.09 | 1.36 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.23× | 0.05 | 0.74 | `counter_irate_total_mul` | `sum(irate(network_bytes{direction="receive"}[5m])) * 8` |
| 14.10× | 0.12 | 1.70 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 13.89× | 0.06 | 0.85 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.81× | 0.11 | 1.57 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 13.67× | 0.06 | 0.75 | `counter_irate_sum_with_labels` | `sum(irate(blockio_operations{op="write"}[5m]))` |
| 13.48× | 0.18 | 2.40 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_rx{source="cachecannon"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 26.53× | 0.09 | 2.27 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 22.92× | 0.22 | 5.11 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 16.32× | 0.10 | 1.62 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 15.17× | 0.11 | 1.61 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 13.36× | 0.11 | 1.47 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 12.77× | 0.12 | 1.49 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 12.40× | 0.12 | 1.44 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 12.20× | 0.11 | 1.39 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 12.17× | 0.13 | 1.53 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 12.04× | 0.13 | 1.58 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.05 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_dram_bandwidth_utilization) / 100` |
| 0.01× | 0.06 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.09 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="decode_forward",source="sglang-decode"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 34.83× | 0.39 | 13.57 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 30.74× | 0.19 | 5.70 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 28.14× | 0.29 | 8.09 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 27.08× | 0.26 | 7.00 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 22.13× | 0.29 | 6.39 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 21.83× | 0.78 | 16.97 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.87× | 0.35 | 7.34 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 15.84× | 0.29 | 4.65 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 15.77× | 0.29 | 4.61 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 15.13× | 0.29 | 4.43 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.14 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.42× | 0.16 | 2.30 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 14.03× | 0.20 | 2.81 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.89× | 0.21 | 2.87 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 13.65× | 0.17 | 2.37 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 13.61× | 0.29 | 4.00 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 13.52× | 0.17 | 2.29 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.48× | 0.35 | 4.74 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 13.43× | 0.20 | 2.71 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 13.41× | 0.54 | 7.22 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 13.20× | 0.07 | 0.93 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="event"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang"}[5s]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 13.98× | 0.21 | 2.95 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 12.24× | 0.22 | 2.71 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 12.21× | 0.23 | 2.77 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 11.62× | 0.22 | 2.57 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 11.29× | 0.22 | 2.43 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 11.27× | 0.24 | 2.72 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 11.24× | 0.22 | 2.51 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 11.03× | 0.23 | 2.57 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 10.64× | 0.24 | 2.53 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 10.58× | 0.11 | 1.19 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang"}` |
| 0.01× | 0.09 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-decode"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_generation_duration_seconds{source="sglang-router"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang"}` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 19.19× | 0.23 | 4.50 | `counter_ratio_generic` | `sum(irate(cpu_branch_misses[5m])) / sum(irate(cpu_branch_instructions[5m]))` |
| 17.33× | 0.18 | 3.07 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 14.03× | 0.29 | 4.13 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 13.99× | 0.18 | 2.56 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 13.79× | 0.20 | 2.74 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.78× | 0.19 | 2.61 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.74× | 0.18 | 2.46 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 13.71× | 0.21 | 2.82 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 13.16× | 0.15 | 2.03 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 13.09× | 0.44 | 5.80 | `rezolus_cpu_aperf_chain_per_id` | `sum by (id) (irate(cpu_tsc[5m])) * sum by (id) (irate(cpu_aperf[5m])) / sum by (id) (irate(cpu_mperf…` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="event"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |

