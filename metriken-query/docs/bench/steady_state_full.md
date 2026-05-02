# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 329.6 ms | 28.9 ms | 7.4 ms | 258.9 ms | 1.7 ms | 244.5 ms |
| `AB_base_pin.parquet` | 3.1 | 320.2 ms | 11.7 ms | 3.7 ms | 212.4 ms | 0.1 ms | 226.7 ms |
| `AB_level.parquet` | 3.3 | 340.8 ms | 9.9 ms | 2.9 ms | 225.7 ms | 0.2 ms | 226.8 ms |
| `AB_level_pin.parquet` | 2.8 | 314.7 ms | 9.7 ms | 3.0 ms | 223.9 ms | 0.2 ms | 248.3 ms |
| `cachecannon.parquet` | 3.8 | 359.4 ms | 9.6 ms | 3.1 ms | 275.5 ms | 0.2 ms | 279.0 ms |
| `demo.parquet` | 1.2 | 110.7 ms | 10.2 ms | 3.2 ms | 101.2 ms | 0.2 ms | 118.6 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 753.8 ms | 11.2 ms | 3.0 ms | 888.9 ms | 0.4 ms | 954.3 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2628.9 ms | 26.0 ms | 3.3 ms | 3362.3 ms | 0.9 ms | 3215.5 ms |
| `sglang_gemma3.parquet` | 2.1 | 289.0 ms | 25.2 ms | 2.8 ms | 491.9 ms | 0.8 ms | 479.4 ms |
| `vllm.parquet` | 4.0 | 348.4 ms | 19.6 ms | 2.6 ms | 444.7 ms | 1.1 ms | 496.9 ms |
| `vllm_gemma3.parquet` | 2.1 | 327.2 ms | 19.7 ms | 3.1 ms | 435.4 ms | 0.6 ms | 482.2 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.113 | 0.737 | 6.51× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.116 | 0.729 | 6.27× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.122 | 0.818 | 6.73× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.116 | 0.792 | 6.84× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.154 | 1.294 | 8.43× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.081 | 0.532 | 6.58× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.174 | 1.329 | 7.66× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.952 | 4.680 | 4.91× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.180 | 1.639 | 9.08× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.237 | 1.684 | 7.10× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.155 | 1.259 | 8.11× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 5.93 | 16.80× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.43 | 12.72× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.49 | 12.49× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 2.39 | 11.88× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.46 | 11.75× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.64 | 11.45× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.14 | 1.54 | 11.37× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.25 | 11.11× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 1.99 | 10.94× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.21 | 2.30 | 10.90× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.80 | 10.85× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.61 | 10.71× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.96 | 10.63× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.93 | 9.99× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 1.73 | 9.91× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.92 | 9.78× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.86 | 9.72× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.35 | 9.28× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.17 | 8.61× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.85 | 8.56× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.56 | 8.48× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.29 | 7.12× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 2.42 | 6.22× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.33 | 3.19× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.20 | 2.49× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.19 | 2.05× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.09 | 1.44× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.39× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.31× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.26× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.13 | 0.02 | 0.15× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.08× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 5.29 | 14.91× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.55 | 14.32× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.65 | 12.09× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.31 | 12.00× |
| `counter_ratio_scaled` | 1 | 4 | 0.19 | 2.21 | 11.56× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.27 | 11.35× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.02 | 11.28× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.60 | 10.76× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.14 | 1.50 | 10.70× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 1.86 | 10.47× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 1.07 | 10.39× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.14 | 10.38× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.68 | 10.35× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 2.34 | 9.59× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.23 | 2.11 | 9.07× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.66 | 8.55× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.91 | 8.10× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.09 | 8.06× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.54 | 8.04× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.94 | 7.53× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.31 | 7.06× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.28 | 6.11× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 2.35 | 5.86× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.35 | 2.95× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.18 | 2.03× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.93× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.08 | 1.25× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.03 | 0.28× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.25× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 5.70 | 16.10× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.79 | 14.85× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.58 | 14.41× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.71 | 12.56× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.62 | 12.08× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.58 | 11.70× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 1.34 | 11.26× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 2.37 | 11.22× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.60 | 11.00× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.56 | 10.78× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.26 | 2.81 | 10.74× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.03 | 10.27× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 1.06 | 9.91× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.79 | 9.65× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 1.05 | 9.43× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 2.08 | 9.29× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 0.94 | 9.04× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.42 | 8.54× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.47 | 8.34× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.66 | 8.31× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.82 | 7.60× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 2.68 | 6.79× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.31 | 6.56× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.41 | 3.38× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.24 | 2.63× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.20 | 2.32× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.11 | 1.38× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.12 | 0.02 | 0.13× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.09 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.38 | 5.81 | 15.40× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.61 | 14.63× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 2.35 | 13.87× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.74 | 13.08× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.82 | 13.07× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.44 | 12.57× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 1.02 | 12.47× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.46 | 12.16× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 2.06 | 11.42× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 1.17 | 11.38× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.70 | 11.31× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.36 | 11.14× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 2.04 | 11.09× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 2.65 | 10.59× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.65 | 10.42× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 1.05 | 9.87× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.24 | 9.51× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.99 | 9.47× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.64 | 8.85× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.34 | 8.10× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.72 | 7.79× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.31 | 6.89× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.41 | 2.68 | 6.59× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.44 | 3.77× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.20 | 2.24× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.26 | 2.07× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.10 | 1.68× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.03 | 0.35× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.32× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.25× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 8.96 | 23.26× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.28 | 4.82 | 17.01× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.24 | 4.01 | 16.77× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 2.73 | 15.24× |
| `counter_ratio_generic` | 2 | 8 | 0.54 | 8.16 | 15.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.24 | 3.43 | 14.13× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.31 | 4.24 | 13.63× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.96 | 12.81× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 1.33 | 12.18× |
| `counter_total_sum_generic` | 11 | 44 | 0.13 | 1.53 | 11.95× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.76 | 9.01 | 11.83× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.39 | 4.13 | 10.70× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.82 | 10.39× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.10 | 1.04 | 10.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.70 | 6.99 | 10.00× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.20 | 8.74× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.76 | 8.37× |
| `counter_ratio_scaled` | 1 | 4 | 0.62 | 5.11 | 8.19× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.36 | 11.10 | 8.14× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.89 | 7.13× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.43 | 6.65× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.44 | 6.48× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.40 | 2.61× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.20 | 2.53× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.22 | 2.26× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.08 | 1.06× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.13 | 0.11 | 0.87× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.20× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.01 | 0.13× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.24 | 0.02 | 0.08× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.01 | 0.07× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.33 | 0.02 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.40 | 8.33 | 20.68× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.78 | 14.48× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.83 | 14.46× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.19 | 2.67 | 14.03× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.82 | 13.84× |
| `counter_ratio_generic` | 2 | 8 | 0.15 | 2.09 | 13.72× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.76 | 13.61× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.42 | 12.71× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.78 | 11.80× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 1.39 | 11.63× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.54 | 11.02× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 0.68 | 10.30× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.52 | 9.64× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.09 | 0.86 | 9.39× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.37 | 8.42× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 3.31 | 7.56× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 0.93 | 6.29× |
| `gauge_bare` | 10 | 40 | 0.03 | 0.16 | 4.43× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.45 | 4.15× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.21 | 2.83× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.13 | 1.61× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.10 | 1.61× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.19 | 1.48× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.55× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.02 | 0.37× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.29× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.27× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.13× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.20 | 7.54 | 36.82× |
| `counter_irate_total_scaled` | 1 | 4 | 0.10 | 1.27 | 13.35× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.19 | 2.47 | 12.71× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.24 | 3.09 | 12.70× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.75 | 12.56× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.36 | 4.28 | 11.88× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.25 | 2.93 | 11.64× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.27 | 2.89 | 10.91× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 2.16 | 9.98× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.36 | 9.98× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.10 | 0.92 | 9.64× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.68 | 9.52× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.90 | 9.46× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.24 | 2.27 | 9.41× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.12 | 1.13 | 9.26× |
| `counter_ratio_scaled` | 1 | 4 | 0.32 | 2.96 | 9.19× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.42 | 3.73 | 8.90× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.83 | 8.77× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.70 | 8.43× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.11 | 0.80 | 7.62× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.14 | 1.08 | 7.60× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.51 | 7.27× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.94 | 6.22 | 6.59× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.06 | 6.39× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.42 | 6.06× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.10 | 0.57 | 5.50× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.46 | 5.27× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.44 | 5.22× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.35 | 4.86× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.35 | 4.30× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.17 | 1.90× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.19 | 1.80× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.26× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.10 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.11 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.01 | 0.11× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.04× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_generic` | 2 | 8 | 0.10 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.11 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.21 | 15.20 | 72.78× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.30 | 21.49 | 71.43× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.73 | 25.17 | 34.64× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.30 | 6.34 | 21.24× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.48 | 6.74 | 14.13× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.13 | 1.79 | 13.28× |
| `counter_total_sum_generic` | 11 | 44 | 0.34 | 3.76 | 11.20× |
| `counter_irate_total_scaled` | 1 | 4 | 0.20 | 2.18 | 11.09× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 1.06 | 11.63 | 11.00× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.81 | 8.02 | 9.91× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.08 | 10.55 | 9.80× |
| `gauge_subtract` | 1 | 4 | 0.21 | 1.92 | 8.97× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.79 | 6.99 | 8.83× |
| `memory_util_pct` | 1 | 4 | 0.19 | 1.62 | 8.73× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.71 | 5.89 | 8.32× |
| `gauge_bare` | 10 | 40 | 0.11 | 0.87 | 8.09× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.13 | 8.78 | 7.80× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.17 | 1.31 | 7.75× |
| `counter_irate_total_mul` | 2 | 8 | 0.23 | 1.78 | 7.67× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.40 | 9.48 | 6.79× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 3.96 | 26.73 | 6.75× |
| `counter_ratio_scaled` | 1 | 4 | 0.90 | 6.01 | 6.71× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.21 | 1.41 | 6.63× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.21 | 1.39 | 6.60× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.99 | 18.84 | 6.30× |
| `counter_ratio_generic` | 2 | 8 | 0.94 | 5.64 | 6.01× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.31 | 13.66 | 5.92× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.38 | 2.13 | 5.61× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.16 | 27.59 | 5.35× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.44 | 2.17 | 4.91× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.15 | 0.74 | 4.83× |
| `gauge_max_bare` | 1 | 4 | 0.12 | 0.58 | 4.64× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.14 | 0.65 | 4.58× |
| `gauge_sum_bare` | 2 | 8 | 0.15 | 0.65 | 4.39× |
| `gauge_sum_with_labels` | 5 | 20 | 0.12 | 0.54 | 4.37× |
| `counter_rate_bare_generic` | 2 | 8 | 0.69 | 2.72 | 3.96× |
| `gauge_sum_scaled` | 1 | 4 | 0.15 | 0.51 | 3.33× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.21 | 1.44× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.14 | 0.19 | 1.40× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.27 | 0.37 | 1.36× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.21× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.01 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.13 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.29 | 6.27 | 21.64× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.39 | 7.64 | 19.55× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.62 | 11.21 | 18.16× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.24 | 3.92 | 16.47× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.84 | 13.72 | 16.24× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.58 | 8.89 | 15.44× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.45 | 6.90 | 15.39× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.52 | 7.65 | 14.75× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.25 | 3.73 | 14.65× |
| `counter_ratio_scaled` | 1 | 4 | 0.25 | 3.57 | 14.19× |
| `counter_ratio_generic` | 2 | 8 | 0.38 | 5.30 | 13.87× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 1.10 | 13.17× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.83 | 13.03× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 1.33 | 12.88× |
| `counter_total_sum_generic` | 11 | 44 | 0.18 | 2.15 | 12.03× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.52 | 5.64 | 10.95× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.83 | 10.89× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.31 | 3.32 | 10.80× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.12 | 1.32 | 10.63× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.21 | 2.11 | 10.21× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.35 | 3.16 | 9.10× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.28 | 8.96× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.10 | 0.87 | 8.63× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 1.07 | 8.59× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 0.87 | 7.91× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.08 | 0.46 | 6.12× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.12 | 0.71 | 6.12× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.10 | 0.50 | 4.82× |
| `gauge_sum_bare` | 2 | 8 | 0.10 | 0.45 | 4.51× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.10 | 0.42 | 4.13× |
| `gauge_avg_scaled` | 6 | 24 | 0.09 | 0.38 | 4.09× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.41 | 3.71× |
| `gauge_bare` | 10 | 40 | 0.09 | 0.32 | 3.43× |
| `gauge_sum_scaled` | 1 | 4 | 0.12 | 0.38 | 3.33× |
| `gauge_sum_with_labels` | 5 | 20 | 0.11 | 0.36 | 3.22× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.46 | 3.05× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.25 | 1.63× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.15 | 0.22 | 1.48× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 0.09 | 0.76× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 0.11 | 0.76× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 0.10 | 0.66× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.14 | 0.03 | 0.19× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.10 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.11 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.11 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 1.11 | 13.45 | 12.16× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.82 | 9.45 | 11.54× |
| `counter_ratio_generic` | 2 | 8 | 0.36 | 4.13 | 11.39× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.48 | 5.47 | 11.35× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.30 | 3.33 | 11.13× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.37 | 4.08 | 10.94× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.51 | 5.39 | 10.68× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.19 | 1.96 | 10.51× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.76 | 7.70 | 10.10× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.15 | 1.46 | 10.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.92 | 9.83× |
| `counter_total_sum_generic` | 11 | 44 | 0.20 | 1.96 | 9.69× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.81 | 7.75 | 9.60× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.36 | 3.40 | 9.53× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.60 | 5.52 | 9.28× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 0.92 | 8.60× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.85 | 8.51× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.24 | 1.98 | 8.32× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.17 | 1.39 | 8.05× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.69 | 7.74× |
| `memory_util_pct` | 1 | 4 | 0.11 | 0.80 | 7.44× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.26 | 1.87 | 7.20× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.22 | 7.15× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.77 | 5.49 | 7.10× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.11 | 0.74 | 6.79× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.20 | 1.36 | 6.64× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.19 | 1.26 | 6.63× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.49 | 5.64× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.52 | 4.77× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.10 | 0.48 | 4.72× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.42 | 4.56× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.11 | 0.45 | 4.11× |
| `gauge_sum_with_labels` | 5 | 20 | 0.12 | 0.44 | 3.64× |
| `gauge_avg_scaled` | 6 | 24 | 0.13 | 0.47 | 3.54× |
| `gauge_bare` | 10 | 40 | 0.10 | 0.29 | 2.93× |
| `gauge_sum_bare` | 2 | 8 | 0.17 | 0.46 | 2.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.22 | 1.73× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.27 | 0.47 | 1.72× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.14 | 0.22 | 1.54× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.14 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.12 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.10 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.11 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.14 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.14 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.23 | 3.58 | 15.78× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.17 | 2.39 | 13.67× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 3.10 | 13.35× |
| `counter_ratio_generic` | 2 | 8 | 0.28 | 3.67 | 13.16× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.75 | 9.63 | 12.81× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.56 | 7.10 | 12.71× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.36 | 4.57 | 12.71× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 0.98 | 12.44× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.54 | 6.47 | 12.07× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.38 | 4.55 | 11.86× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.37 | 11.80× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 2.56 | 11.67× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.76 | 11.30× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 2.22 | 11.24× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.75 | 11.20× |
| `counter_total_sum_generic` | 11 | 44 | 0.15 | 1.65 | 10.88× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.80 | 10.66× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.77 | 10.31× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.54 | 5.44 | 10.16× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 1.47 | 9.63× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 1.41 | 9.61× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 0.93 | 9.61× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.51 | 4.69 | 9.16× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.10 | 9.01× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.19 | 1.67 | 8.94× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.18 | 1.48 | 8.32× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.98 | 8.18× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.65 | 7.32× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.81 | 6.84× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.43 | 6.28× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.37 | 5.69× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.34 | 5.33× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.43 | 5.32× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.40 | 5.22× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.35 | 4.98× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.43 | 4.98× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.36 | 4.86× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.34 | 4.51× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.44 | 2.69× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.20 | 2.05× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.19 | 1.91× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.01 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.80× | 0.35 | 5.93 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.65× | 0.07 | 0.96 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.86× | 0.19 | 2.65 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 13.80× | 0.11 | 1.55 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 13.35× | 0.12 | 1.65 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.59× | 0.12 | 1.53 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 12.50× | 0.13 | 1.68 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 12.49× | 0.12 | 1.49 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 12.33× | 0.11 | 1.39 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="timer"}[5m]))` |
| 12.21× | 0.13 | 1.59 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="used"})` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.44× | 0.11 | 1.68 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.91× | 0.35 | 5.29 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.76× | 0.13 | 1.91 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 14.55× | 0.07 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.30× | 0.24 | 3.17 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 13.25× | 0.11 | 1.48 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 12.58× | 0.04 | 0.57 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 12.34× | 0.13 | 1.62 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.28× | 0.28 | 3.49 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 12.10× | 0.13 | 1.62 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.10× | 0.35 | 5.70 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 15.36× | 0.05 | 0.76 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 14.85× | 0.05 | 0.79 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 14.64× | 0.06 | 0.94 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.43× | 0.12 | 1.75 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.41× | 0.11 | 1.58 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.23× | 0.10 | 1.48 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 14.20× | 0.10 | 1.47 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.90× | 0.18 | 2.51 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 13.42× | 0.26 | 3.51 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.09 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="free"})` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.20× | 0.11 | 1.89 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.09× | 0.06 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 15.99× | 0.10 | 1.65 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.40× | 0.38 | 5.81 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.92× | 0.16 | 2.43 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 14.60× | 0.12 | 1.78 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 14.24× | 0.11 | 1.62 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 14.01× | 0.05 | 0.66 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 13.99× | 0.12 | 1.73 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 13.87× | 0.17 | 2.35 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.04 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 28.88× | 0.16 | 4.50 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 23.26× | 0.39 | 8.96 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 22.31× | 0.13 | 2.88 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 20.63× | 0.35 | 7.29 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 19.20× | 0.38 | 7.26 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 18.37× | 0.37 | 6.89 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 18.20× | 0.28 | 5.11 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 18.19× | 0.36 | 6.47 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 17.69× | 0.36 | 6.39 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 17.32× | 0.31 | 5.36 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="timer"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.08 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.00× | 0.07 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.00× | 0.08 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="event"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 20.68× | 0.40 | 8.33 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 19.23× | 0.16 | 3.14 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 16.95× | 0.07 | 1.12 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 16.79× | 0.14 | 2.36 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 16.78× | 0.10 | 1.70 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 15.79× | 0.11 | 1.80 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.62× | 0.12 | 1.80 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 14.46× | 0.06 | 0.83 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 14.18× | 0.17 | 2.48 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 14.03× | 0.19 | 2.67 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.11 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 36.82× | 0.20 | 7.54 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 28.76× | 0.09 | 2.64 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 17.27× | 0.10 | 1.80 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 16.74× | 0.58 | 9.68 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 16.49× | 0.31 | 5.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_migrations{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.32× | 0.17 | 2.77 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 16.09× | 0.22 | 3.55 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 16.03× | 0.14 | 2.31 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 15.14× | 0.21 | 3.22 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 15.13× | 0.11 | 1.66 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.10 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang-prefill"}` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.05 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_utilization) / 100` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_dram_bandwidth_utilization) / 100` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 72.78× | 0.21 | 15.20 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 71.43× | 0.30 | 21.49 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 35.26× | 0.26 | 9.10 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 34.64× | 0.73 | 25.17 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 31.82× | 0.21 | 6.76 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 28.21× | 0.41 | 11.55 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 22.59× | 0.38 | 8.57 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 21.24× | 0.30 | 6.34 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 19.39× | 0.40 | 7.74 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 17.37× | 0.32 | 5.57 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.14 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.14 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.13 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_dram_bandwidth_utilization) / 100` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.13 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_hits{source="cachecannon"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_utilization) / 100` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_size{op="write"}` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 22.94× | 0.26 | 5.87 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 21.71× | 0.17 | 3.60 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 21.64× | 0.29 | 6.27 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 21.30× | 0.20 | 4.17 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 21.18× | 0.17 | 3.68 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 20.94× | 0.22 | 4.61 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 20.50× | 0.21 | 4.23 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 19.94× | 0.25 | 4.97 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="poll",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 19.77× | 0.17 | 3.40 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 19.55× | 0.39 | 7.64 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.00× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.00× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.00× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.14 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.32× | 0.50 | 7.70 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 15.10× | 0.29 | 4.32 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 15.07× | 0.29 | 4.30 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 14.53× | 0.33 | 4.73 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 14.31× | 0.28 | 3.94 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="to"}[5m]))` |
| 14.18× | 0.27 | 3.89 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 13.92× | 0.28 | 3.95 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 13.59× | 0.21 | 2.82 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 13.58× | 0.40 | 5.39 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 13.37× | 0.38 | 5.08 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_hits{source="cachecannon"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.16 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang"}` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_rx{source="cachecannon"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-decode"}` |
| 0.01× | 0.10 | 0.00 | `gauge_bare` | `syscall_latency` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 19.72× | 0.22 | 4.39 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 19.05× | 0.34 | 6.47 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 18.97× | 0.17 | 3.30 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 18.86× | 0.19 | 3.58 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 18.61× | 0.20 | 3.66 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="local_mm_shootdown"}[5m]))` |
| 17.90× | 0.19 | 3.41 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 17.63× | 0.17 | 3.04 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 17.26× | 0.22 | 3.79 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_migrations{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 17.15× | 0.20 | 3.50 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="rcu"}[5m]))` |
| 16.71× | 0.20 | 3.40 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |

