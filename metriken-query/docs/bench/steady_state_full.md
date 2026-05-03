# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 362.6 ms | 40.3 ms | 12.0 ms | 329.4 ms | 1.7 ms | 287.7 ms |
| `AB_base_pin.parquet` | 3.1 | 326.0 ms | 9.6 ms | 3.0 ms | 244.2 ms | 0.1 ms | 247.1 ms |
| `AB_level.parquet` | 3.3 | 349.2 ms | 11.7 ms | 3.3 ms | 243.7 ms | 0.2 ms | 251.4 ms |
| `AB_level_pin.parquet` | 2.8 | 359.0 ms | 13.1 ms | 3.6 ms | 334.1 ms | 0.2 ms | 290.1 ms |
| `cachecannon.parquet` | 3.8 | 363.2 ms | 8.8 ms | 3.2 ms | 298.6 ms | 0.2 ms | 324.7 ms |
| `demo.parquet` | 1.2 | 114.0 ms | 12.2 ms | 3.3 ms | 102.9 ms | 0.2 ms | 130.2 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 778.0 ms | 10.8 ms | 3.2 ms | 1538.6 ms | 0.4 ms | 1710.5 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2866.0 ms | 25.7 ms | 3.2 ms | 5835.4 ms | 0.8 ms | 3909.5 ms |
| `sglang_gemma3.parquet` | 2.1 | 324.0 ms | 26.5 ms | 3.6 ms | 620.9 ms | 0.7 ms | 582.2 ms |
| `vllm.parquet` | 4.0 | 373.5 ms | 19.2 ms | 3.1 ms | 452.6 ms | 0.9 ms | 487.9 ms |
| `vllm_gemma3.parquet` | 2.1 | 402.9 ms | 24.9 ms | 4.0 ms | 472.3 ms | 1.1 ms | 465.7 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.115 | 0.483 | 4.20× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.120 | 0.509 | 4.25× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.141 | 0.629 | 4.47× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.124 | 0.554 | 4.48× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.168 | 0.960 | 5.73× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.088 | 0.368 | 4.21× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.173 | 0.832 | 4.81× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.886 | 2.600 | 2.93× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.152 | 0.927 | 6.11× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.205 | 1.016 | 4.97× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.159 | 1.022 | 6.43× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.05 | 0.72 | 15.16× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.56 | 14.76× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.67 | 11.20× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.00 | 11.03× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.72 | 10.69× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.18 | 10.63× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 2.59 | 10.55× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.06 | 9.38× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.20 | 8.40× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.04 | 8.34× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 0.94 | 7.92× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.71 | 7.74× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 0.99 | 7.04× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.73 | 6.89× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.42 | 2.80 | 6.71× |
| `counter_ratio_generic` | 2 | 8 | 0.19 | 1.19 | 6.16× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 2.33 | 5.37× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.28 | 4.38× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.47 | 4.29× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.24 | 3.84× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.41 | 3.37× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.24 | 0.70 | 2.94× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.12 | 2.89× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.19 | 2.31× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.20 | 1.63× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.09 | 1.56× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.09 | 1.24× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.30× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.17× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.05 | 0.75 | 15.45× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.47 | 13.38× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.76 | 13.24× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 3.03 | 13.22× |
| `counter_ratio_scaled` | 1 | 4 | 0.19 | 2.18 | 11.26× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.18 | 9.71× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.00 | 9.60× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.74 | 9.53× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.44 | 8.79× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.37 | 8.43× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 0.87 | 7.87× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.76 | 7.87× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 0.85 | 7.18× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.36 | 2.59 | 7.15× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 2.60 | 7.07× |
| `counter_ratio_generic` | 2 | 8 | 0.19 | 1.30 | 6.86× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 0.96 | 6.01× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.17 | 0.78 | 4.55× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.40 | 4.49× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.46 | 4.26× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.25 | 3.97× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.27 | 3.91× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.14 | 3.18× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.13 | 2.35× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.19 | 2.15× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.24 | 1.92× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.09 | 0.87× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.30× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.10 | 0.02 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.18× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.11 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.73 | 15.17× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.77 | 13.83× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.38 | 13.78× |
| `counter_ratio_scaled` | 1 | 4 | 0.26 | 2.92 | 11.42× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.30 | 3.28 | 10.78× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.17 | 1.79 | 10.68× |
| `memory_util_pct` | 1 | 4 | 0.09 | 0.91 | 10.64× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.26 | 9.57× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.50 | 8.35× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 1.24 | 8.35× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.79 | 7.99× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.41 | 3.25 | 7.96× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.25 | 1.97 | 7.92× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.15 | 1.19 | 7.87× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 0.91 | 6.86× |
| `counter_ratio_generic` | 2 | 8 | 0.22 | 1.48 | 6.75× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 1.09 | 5.04× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.73 | 3.40 | 4.67× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 0.58 | 4.34× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.33 | 3.94× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.34 | 3.66× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.13 | 3.04× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.37 | 2.15× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.29 | 2.14× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.23 | 1.92× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.17 | 1.73× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.12 | 1.58× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.11 | 0.03 | 0.25× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.03 | 0.25× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.20× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.09 | 0.01 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.01 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.62 | 14.40× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.73 | 12.47× |
| `counter_ratio_scaled` | 1 | 4 | 0.22 | 2.62 | 12.06× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.28 | 3.26 | 11.84× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.13 | 11.39× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.63 | 10.69× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.58 | 10.52× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.17 | 9.86× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.30 | 9.53× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.27 | 8.77× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.05 | 8.39× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.00 | 8.26× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.80 | 7.54× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.40 | 2.85 | 7.20× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.42 | 2.75 | 6.54× |
| `counter_ratio_generic` | 2 | 8 | 0.21 | 1.24 | 6.03× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 0.79 | 5.61× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.32 | 4.24× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 0.49 | 3.82× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.26 | 3.82× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 0.72 | 3.72× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.44 | 3.37× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.13 | 3.11× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.10 | 2.07× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.22 | 1.93× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.20 | 1.34× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.11 | 0.11 | 1.00× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.28× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.18× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.12 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.37 | 8.98 | 24.25× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 2.43 | 14.64× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.33 | 4.77 | 14.26× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.73 | 7.85 | 10.75× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.30 | 3.13 | 10.44× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.45 | 10.32× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.76 | 10.31× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.29 | 2.68 | 9.18× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.69 | 8.98× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.44 | 3.90 | 8.77× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.37 | 11.69 | 8.54× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.84 | 6.98 | 8.29× |
| `counter_ratio_scaled` | 1 | 4 | 0.65 | 5.12 | 7.94× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.03 | 7.91× |
| `counter_irate_total_mul` | 2 | 8 | 0.15 | 0.84 | 5.75× |
| `counter_ratio_generic` | 2 | 8 | 0.64 | 3.62 | 5.65× |
| `counter_irate_total_scaled` | 1 | 4 | 0.10 | 0.43 | 4.49× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.27 | 1.21 | 4.39× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.13 | 0.45 | 3.51× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.23 | 3.45× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.11 | 0.32 | 2.94× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.39 | 2.32× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.23 | 2.05× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.21 | 1.96× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.13 | 1.48× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.14 | 0.13 | 0.93× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.09 | 0.82× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.11 | 0.02 | 0.19× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.14 | 0.02 | 0.17× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.26 | 0.02 | 0.09× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.32 | 0.02 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.89 | 10.77× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.17 | 1.70 | 10.25× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.23 | 9.93× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.34 | 9.74× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 1.86 | 9.51× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.46 | 9.07× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.16 | 8.93× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 3.95 | 8.61× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.45 | 8.33× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.43 | 3.56 | 8.25× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.26 | 7.93× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 0.82 | 6.17× |
| `counter_irate_total_mul` | 2 | 8 | 0.14 | 0.82 | 5.97× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.35 | 5.83× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.53 | 4.68× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.33 | 4.12× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.33 | 2.83× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.10 | 2.23× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.19 | 2.17× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.13 | 2.06× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.22 | 2.02× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.08 | 1.78× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.12 | 1.33× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.41× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.41× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.05 | 0.01 | 0.28× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.26× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.14× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.04 | 0.00 | 0.09× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.22 | 5.51 | 24.99× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.54 | 15.35× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 1.60 | 13.19× |
| `counter_total_sum_generic` | 11 | 44 | 0.18 | 1.62 | 9.13× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.71 | 8.98× |
| `counter_ratio_scaled` | 1 | 4 | 0.34 | 3.01 | 8.94× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 1.95 | 8.70× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.30 | 2.57 | 8.63× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.73 | 7.88× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.85 | 7.78× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.44 | 3.40 | 7.77× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.15 | 1.13 | 7.35× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.63 | 7.33× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.23 | 1.63 | 6.97× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.55 | 6.96× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.24 | 1.68 | 6.89× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.99 | 6.73 | 6.78× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.10 | 0.68 | 6.63× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.25 | 1.61 | 6.45× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.37 | 6.30× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.05 | 6.09× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.41 | 2.25 | 5.47× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.41 | 4.96× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.20 | 0.86 | 4.25× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.21 | 3.52× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.31 | 3.33× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.20 | 1.88× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.24 | 1.85× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.10 | 1.80× |
| `gauge_sum_bare` | 2 | 8 | 0.10 | 0.14 | 1.49× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.10 | 1.44× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.10 | 1.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.03 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.28× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.11 | 0.03 | 0.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.20× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.10× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.00 | 0.02× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.09 | 0.00 | 0.02× |
| `counter_ratio_generic` | 2 | 8 | 0.11 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.23 | 9.61 | 41.87× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.34 | 13.05 | 38.47× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.78 | 16.17 | 20.66× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.24 | 4.16 | 17.07× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.39 | 4.67 | 11.95× |
| `memory_util_pct` | 1 | 4 | 0.19 | 1.50 | 7.79× |
| `gauge_subtract` | 1 | 4 | 0.15 | 1.17 | 7.73× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.17 | 1.22 | 7.12× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.98 | 6.77 | 6.93× |
| `counter_total_sum_generic` | 11 | 44 | 0.31 | 2.11 | 6.92× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.19 | 1.25 | 6.49× |
| `counter_irate_total_mul` | 2 | 8 | 0.20 | 1.13 | 5.58× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.27 | 7.07 | 5.58× |
| `gauge_bare` | 10 | 40 | 0.11 | 0.62 | 5.51× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.92 | 5.07 | 5.51× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.65 | 3.50 | 5.41× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.99 | 5.20 | 5.25× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.14 | 0.71 | 5.23× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.64 | 3.23 | 5.06× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 3.84 | 19.40 | 5.05× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.54 | 4.96× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.62 | 12.12 | 4.63× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.29 | 1.32 | 4.61× |
| `counter_ratio_scaled` | 1 | 4 | 0.95 | 3.92 | 4.13× |
| `counter_irate_total_scaled` | 1 | 4 | 0.19 | 0.76 | 4.00× |
| `gauge_sum_scaled` | 1 | 4 | 0.11 | 0.42 | 3.92× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.12 | 19.45 | 3.80× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.63 | 2.20 | 3.51× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.44 | 1.50 | 3.37× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.14 | 7.00 | 3.27× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.15 | 0.49 | 3.21× |
| `counter_rate_bare_generic` | 2 | 8 | 0.63 | 1.97 | 3.13× |
| `counter_ratio_generic` | 2 | 8 | 0.84 | 2.17 | 2.59× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.10 | 0.19 | 1.98× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.35 | 1.82× |
| `gauge_sum_bare` | 2 | 8 | 0.12 | 0.21 | 1.78× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.11 | 0.19 | 1.71× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.22 | 1.64× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.14 | 0.21 | 1.52× |
| `gauge_sum_with_labels` | 5 | 20 | 0.11 | 0.14 | 1.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.06 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.20 | 3.42 | 16.84× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.57 | 8.90 | 15.54× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.35 | 5.36 | 15.50× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.68 | 9.82 | 14.45× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 1.05 | 13.93× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.87 | 13.22× |
| `counter_ratio_scaled` | 1 | 4 | 0.25 | 3.20 | 13.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.21 | 2.65 | 12.91× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.52 | 6.38 | 12.27× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.22 | 2.66 | 12.21× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.20 | 2.34 | 11.58× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.19 | 2.14 | 11.44× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 0.95 | 11.36× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.63 | 10.63× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.40 | 3.94 | 9.93× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.51 | 4.84 | 9.42× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.70 | 9.26× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.55 | 9.16× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.13 | 1.13 | 8.40× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.50 | 4.12 | 8.30× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.95 | 7.70× |
| `counter_ratio_generic` | 2 | 8 | 0.30 | 2.28 | 7.56× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 0.76 | 5.76× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.43 | 5.16× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.26 | 1.23 | 4.73× |
| `gauge_max_bare` | 1 | 4 | 0.08 | 0.34 | 4.46× |
| `gauge_avg_scaled` | 6 | 24 | 0.10 | 0.43 | 4.38× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.31 | 4.19× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.29 | 2.89× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.36 | 2.08× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.11 | 1.81× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.10 | 1.68× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.19 | 1.57× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.10 | 1.44× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.20 | 1.36× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.08 | 1.22× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.09 | 1.13× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 0.12 | 0.94× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 0.11 | 0.91× |
| `gauge_sum_bare` | 2 | 8 | 0.11 | 0.10 | 0.89× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 0.10 | 0.84× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.23× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 1.10 | 12.16× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.44 | 5.35 | 12.12× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.23 | 2.80 | 11.98× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.25 | 2.92 | 11.76× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.99 | 11.27 | 11.37× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.80 | 8.54 | 10.61× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.62 | 6.29 | 10.20× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.46 | 4.48 | 9.72× |
| `counter_total_sum_generic` | 11 | 44 | 0.21 | 1.90 | 8.93× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.23 | 1.88 | 8.36× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.73 | 5.70 | 7.76× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.58 | 7.55× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.13 | 0.99 | 7.50× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.30 | 2.24 | 7.49× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.65 | 7.13× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.55 | 6.98× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 0.83 | 6.30× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 1.00 | 6.11× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.72 | 4.33 | 5.98× |
| `counter_ratio_generic` | 2 | 8 | 0.37 | 2.19 | 5.95× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.67 | 5.45× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.40 | 2.16 | 5.43× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.16 | 0.83 | 5.08× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.36 | 4.93× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.18 | 0.87 | 4.83× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.43 | 3.91× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 0.37 | 3.50× |
| `gauge_avg_scaled` | 6 | 24 | 0.12 | 0.39 | 3.27× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.17 | 0.50 | 2.95× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.25 | 2.41× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.20 | 0.42 | 2.06× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.24 | 1.82× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.19 | 1.75× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.11 | 1.58× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.36× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.09 | 1.34× |
| `gauge_bare` | 10 | 40 | 0.08 | 0.10 | 1.19× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.08 | 0.93× |
| `gauge_sum_bare` | 2 | 8 | 0.13 | 0.11 | 0.87× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.09 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.12 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.34 | 6.37 | 18.60× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.17 | 2.83 | 16.59× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 1.29 | 16.40× |
| `counter_ratio_scaled` | 1 | 4 | 0.22 | 3.36 | 15.20× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 1.14 | 15.12× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 3.27 | 14.46× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.71 | 10.27 | 14.46× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.66 | 9.32 | 14.18× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.87 | 13.16× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.52 | 6.85 | 13.09× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.22 | 2.91 | 13.06× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.21 | 2.71 | 12.93× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.38 | 4.78 | 12.68× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.74 | 12.04× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.73 | 11.75× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 1.04 | 11.36× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 2.07 | 11.29× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.86 | 9.62× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.54 | 4.98 | 9.17× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.76 | 8.04× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.73 | 7.15× |
| `counter_ratio_generic` | 2 | 8 | 0.31 | 2.18 | 6.95× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.67 | 4.52 | 6.79× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 0.92 | 6.02× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.15 | 0.89 | 5.90× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.41 | 5.71× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.16 | 0.92 | 5.66× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.35 | 4.74× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.44 | 4.15× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 0.58 | 4.08× |
| `gauge_avg_scaled` | 6 | 24 | 0.11 | 0.43 | 4.08× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.26 | 3.25× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.49 | 2.57× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.24 | 2.54× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.25 | 2.18× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.11 | 1.85× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.11 | 1.65× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.10 | 1.53× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.46× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.11 | 1.45× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.12 | 1.40× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.08 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.86× | 0.11 | 1.67 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.16× | 0.05 | 0.72 | `gauge_subtract` | `memory_total - memory_available` |
| 13.53× | 0.12 | 1.56 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 12.89× | 0.10 | 1.29 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 12.87× | 0.07 | 0.95 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.09× | 0.11 | 1.30 | `counter_total_sum_generic` | `sum(irate(cpu_tlb_flush[5m]))` |
| 11.88× | 0.14 | 1.62 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 11.24× | 0.06 | 0.63 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 11.20× | 0.06 | 0.67 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 11.03× | 0.09 | 1.00 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_pcie_throughput{direction="receive"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.55× | 0.11 | 1.71 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.45× | 0.05 | 0.75 | `gauge_subtract` | `memory_total - memory_available` |
| 14.35× | 0.07 | 1.00 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.24× | 0.06 | 0.76 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 13.22× | 0.23 | 3.03 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 12.80× | 0.10 | 1.33 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 12.24× | 0.13 | 1.62 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 11.94× | 0.06 | 0.75 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 11.38× | 0.13 | 1.47 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 11.30× | 0.11 | 1.20 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `gauge_avg_scaled` | `avg(gpu_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="memory"})` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.88× | 0.11 | 1.97 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 17.88× | 0.11 | 2.03 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.77× | 0.08 | 1.38 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.15× | 0.18 | 2.58 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 13.83× | 0.06 | 0.77 | `gauge_subtract` | `memory_total - memory_available` |
| 13.39× | 0.16 | 2.13 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 13.22× | 0.21 | 2.78 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 13.10× | 0.22 | 2.90 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 12.98× | 0.20 | 2.63 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 12.74× | 0.15 | 1.92 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.15 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.64× | 0.11 | 2.09 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.96× | 0.07 | 1.15 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.96× | 0.10 | 1.49 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 12.47× | 0.06 | 0.73 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.06× | 0.22 | 2.62 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 11.84× | 0.28 | 3.26 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 11.42× | 0.14 | 1.57 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 10.78× | 0.09 | 0.99 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |
| 10.69× | 0.06 | 0.63 | `gauge_subtract` | `memory_total - memory_available` |
| 10.52× | 0.15 | 1.58 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_tpot_seconds{source="sglang-router"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 24.25× | 0.37 | 8.98 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.61× | 0.21 | 4.28 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 18.63× | 0.17 | 3.09 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 15.65× | 0.16 | 2.49 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 14.80× | 0.38 | 5.60 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 14.36× | 0.33 | 4.81 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.35× | 0.40 | 5.80 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 14.26× | 0.10 | 1.45 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.04× | 0.20 | 2.79 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 13.97× | 0.38 | 5.33 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="success",source="llm-perf"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.07 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_avg_scaled` | `avg(gpu_memory_utilization) / 100` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.82× | 0.08 | 1.40 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 16.22× | 0.37 | 6.02 | `counter_irate_by_id_generic` | `sum by (id) (irate(softirq[5m]))` |
| 15.86× | 0.06 | 0.89 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 13.24× | 0.15 | 1.96 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 12.66× | 0.14 | 1.78 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 12.22× | 0.08 | 1.03 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |
| 11.76× | 0.13 | 1.57 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 11.45× | 0.14 | 1.60 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 11.30× | 0.16 | 1.75 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 10.46× | 0.11 | 1.16 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.13 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 24.99× | 0.22 | 5.51 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.04× | 0.11 | 2.20 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 16.74× | 0.10 | 1.61 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 15.81× | 0.10 | 1.61 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 15.35× | 0.10 | 1.54 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 13.94× | 0.13 | 1.75 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 13.32× | 0.12 | 1.61 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 13.27× | 0.12 | 1.60 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 12.35× | 0.13 | 1.67 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 11.42× | 0.12 | 1.33 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.16 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 41.87× | 0.23 | 9.61 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 38.47× | 0.34 | 13.05 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 20.85× | 0.20 | 4.16 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 20.66× | 0.78 | 16.17 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 18.57× | 0.26 | 4.81 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 17.82× | 0.43 | 7.71 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 17.75× | 0.24 | 4.28 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 15.72× | 0.29 | 4.58 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 14.99× | 0.28 | 4.23 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 14.43× | 0.31 | 4.53 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.08 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang-prefill"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.04× | 0.16 | 2.65 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 16.84× | 0.20 | 3.42 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 15.54× | 0.57 | 8.90 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 15.50× | 0.35 | 5.36 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 15.24× | 0.19 | 2.95 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 14.45× | 0.68 | 9.82 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 14.03× | 0.17 | 2.44 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 13.93× | 0.08 | 1.05 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 13.87× | 0.17 | 2.39 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 13.60× | 0.14 | 1.91 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 13.32× | 0.20 | 2.63 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 12.16× | 0.09 | 1.10 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 12.12× | 0.44 | 5.35 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 11.98× | 0.23 | 2.80 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 11.76× | 0.25 | 2.92 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 11.56× | 0.21 | 2.43 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 11.37× | 0.99 | 11.27 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 11.14× | 0.08 | 0.85 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 11.08× | 0.10 | 1.10 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |
| 10.97× | 0.39 | 4.27 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.20 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_hits{source="cachecannon"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |
| 0.01× | 0.12 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.10 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.10 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang"}` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.60× | 0.34 | 6.37 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 17.24× | 0.16 | 2.83 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 16.40× | 0.08 | 1.29 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 16.27× | 0.17 | 2.78 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 15.65× | 0.18 | 2.83 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 15.30× | 0.14 | 2.21 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 15.20× | 0.22 | 3.36 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 15.12× | 0.18 | 2.69 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 15.12× | 0.08 | 1.14 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 14.92× | 0.18 | 2.71 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="event"}` |

