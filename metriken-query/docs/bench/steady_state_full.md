# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 340.9 ms | 36.2 ms | 5.2 ms | 260.1 ms | 0.6 ms | 240.9 ms |
| `AB_base_pin.parquet` | 3.1 | 311.6 ms | 9.2 ms | 2.9 ms | 218.0 ms | 0.1 ms | 232.6 ms |
| `AB_level.parquet` | 3.3 | 332.1 ms | 9.2 ms | 3.2 ms | 234.9 ms | 0.2 ms | 237.2 ms |
| `AB_level_pin.parquet` | 2.8 | 324.4 ms | 8.9 ms | 3.5 ms | 203.5 ms | 0.2 ms | 231.4 ms |
| `cachecannon.parquet` | 3.8 | 355.3 ms | 9.5 ms | 3.4 ms | 268.7 ms | 0.2 ms | 281.2 ms |
| `demo.parquet` | 1.2 | 110.6 ms | 9.3 ms | 2.5 ms | 109.4 ms | 0.2 ms | 112.2 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 700.1 ms | 9.7 ms | 2.8 ms | 1041.6 ms | 0.4 ms | 1003.3 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 3045.7 ms | 21.1 ms | 3.1 ms | 4354.4 ms | 0.8 ms | 3791.5 ms |
| `sglang_gemma3.parquet` | 2.1 | 331.0 ms | 28.5 ms | 3.7 ms | 597.2 ms | 0.7 ms | 507.7 ms |
| `vllm.parquet` | 4.0 | 387.3 ms | 19.6 ms | 3.3 ms | 446.7 ms | 0.9 ms | 507.6 ms |
| `vllm_gemma3.parquet` | 2.1 | 350.5 ms | 19.1 ms | 2.8 ms | 468.2 ms | 0.9 ms | 484.7 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.115 | 0.670 | 5.82× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.115 | 0.643 | 5.61× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.119 | 0.706 | 5.94× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.112 | 0.646 | 5.76× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.155 | 1.177 | 7.60× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.074 | 0.389 | 5.22× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.187 | 1.217 | 6.50× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.974 | 3.896 | 4.00× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.154 | 1.226 | 7.99× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.213 | 1.371 | 6.45× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.166 | 1.331 | 8.04× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.05 | 0.77 | 16.92× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.81 | 15.66× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.56 | 15.18× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.21 | 2.83 | 13.16× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 2.37 | 12.99× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 4.64 | 12.61× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 1.08 | 12.34× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.34 | 11.17× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.72 | 11.05× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.25 | 10.92× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.33 | 10.70× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.08 | 9.33× |
| `counter_ratio_generic` | 2 | 8 | 0.21 | 1.93 | 9.26× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.39 | 9.08× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.84 | 8.80× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.41 | 3.54 | 8.67× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 1.05 | 7.50× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.77 | 7.42× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.26 | 1.71 | 6.59× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.40 | 5.78× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.35 | 5.12× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.41 | 4.25× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.12 | 3.20× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.22 | 2.56× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.10 | 2.40× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.24 | 1.83× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.09 | 1.25× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.37× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.29× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.13 | 0.02 | 0.18× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.15× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.05 | 0.76 | 16.58× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.54 | 14.98× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.75 | 13.85× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.60 | 11.86× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 2.45 | 11.62× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.94 | 11.34× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.38 | 4.26 | 11.15× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.49 | 11.13× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 1.00 | 10.58× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.19 | 10.44× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.13 | 10.43× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.64 | 10.28× |
| `counter_ratio_generic` | 2 | 8 | 0.21 | 1.91 | 9.12× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 1.60 | 9.09× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.23 | 8.42× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.80 | 8.39× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 3.03 | 7.81× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.70 | 7.19× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.44 | 5.98× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.31 | 4.46× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.21 | 0.91 | 4.34× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.36 | 3.56× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.12 | 3.18× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.21 | 2.48× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.20 | 2.44× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.09 | 2.34× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.11 | 1.45× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.31× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.30× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.29× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.03 | 0.29× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.78 | 17.45× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 3.50 | 15.06× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.72 | 14.44× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.72 | 12.50× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.40 | 4.85 | 12.04× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.85 | 11.31× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 1.95 | 11.02× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.46 | 10.83× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.43 | 10.50× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.18 | 1.86 | 10.48× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 2.18 | 10.47× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.52 | 10.42× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.97 | 10.24× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 1.08 | 9.61× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.83 | 8.26× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.32 | 8.21× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.56 | 8.08× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.41 | 6.93× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.15 | 1.00 | 6.58× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.42 | 2.60 | 6.21× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.32 | 4.94× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.13 | 3.16× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.43 | 2.67× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.21 | 2.31× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.27 | 2.03× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.09 | 1.84× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.11 | 1.82× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.34× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.33× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.30× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.20× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.57 | 16.19× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.68 | 14.77× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.74 | 12.85× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.42 | 4.99 | 11.98× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.62 | 11.86× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.14 | 1.60 | 11.24× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 1.94 | 11.15× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.96 | 10.95× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.10 | 1.12 | 10.84× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.38 | 10.70× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 2.26 | 10.62× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.74 | 9.98× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 1.07 | 9.93× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.15 | 9.88× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.26 | 9.50× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.93 | 8.78× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.71 | 7.59× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 2.53 | 6.55× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.39 | 6.20× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 1.36 | 6.14× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.32 | 4.89× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.39 | 3.17× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.12 | 3.09× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.22 | 2.80× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.10 | 2.40× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.20 | 2.30× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.11 | 1.44× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.24× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.24× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.11 | 0.03 | 0.23× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.41 | 9.14 | 22.47× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.26 | 4.23 | 16.13× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 2.61 | 15.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.72 | 10.21 | 14.26× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.26 | 3.66 | 14.12× |
| `counter_ratio_generic` | 2 | 8 | 0.59 | 8.24 | 13.87× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.96 | 13.81× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.32 | 4.30 | 13.50× |
| `counter_total_sum_generic` | 11 | 44 | 0.12 | 1.38 | 11.78× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.23 | 2.64 | 11.38× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.41 | 4.26 | 10.44× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.75 | 6.98 | 9.27× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.27 | 11.08 | 8.69× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 1.27 | 8.68× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.80 | 8.53× |
| `counter_ratio_scaled` | 1 | 4 | 0.57 | 4.65 | 8.17× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.90 | 7.77× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.11 | 0.67 | 6.20× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 0.67 | 6.03× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.40 | 4.86× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.48 | 3.85× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.27 | 3.71× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.28 | 2.40× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.23 | 2.08× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.10 | 1.58× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.11 | 0.11 | 1.04× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 0.07 | 0.59× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.25× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.10 | 0.02 | 0.20× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.13 | 0.02 | 0.14× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.19 | 0.02 | 0.11× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.27 | 0.02 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.11 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.10 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.38 | 5.26 | 13.88× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.37 | 12.47× |
| `counter_ratio_generic` | 2 | 8 | 0.15 | 1.80 | 12.23× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.53 | 11.90× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.18 | 2.14 | 11.57× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.14 | 1.55 | 11.17× |
| `counter_total_sum_generic` | 11 | 44 | 0.06 | 0.67 | 10.52× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.50 | 10.33× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.67 | 10.00× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.90 | 9.95× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 1.14 | 9.53× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.44 | 8.89× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.81 | 7.95× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.44 | 7.34× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 2.75 | 7.01× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.07 | 0.41 | 6.11× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.34 | 2.99× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.26 | 2.94× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.11 | 2.88× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.20 | 2.51× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.07 | 1.82× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.09 | 1.60× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.10 | 1.40× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.49× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.42× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.21× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.01 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.12× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.24 | 6.31 | 26.40× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.99 | 13.04× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.36 | 12.80× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 2.77 | 12.51× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 1.06 | 11.39× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.24 | 2.55 | 10.80× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.26 | 2.80 | 10.70× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.28 | 2.88 | 10.43× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.21 | 2.17 | 10.26× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.83 | 10.23× |
| `counter_total_sum_generic` | 11 | 44 | 0.17 | 1.69 | 9.92× |
| `counter_ratio_scaled` | 1 | 4 | 0.36 | 3.36 | 9.28× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.86 | 9.11× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.10 | 0.95 | 9.03× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.30 | 2.51 | 8.35× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.16 | 1.35 | 8.33× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.98 | 7.95× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.51 | 3.99 | 7.84× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.01 | 7.20 | 7.11× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.47 | 6.51× |
| `gauge_subtract` | 1 | 4 | 0.13 | 0.85 | 6.44× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.64 | 3.91 | 6.13× |
| `counter_rate_bare_generic` | 2 | 8 | 0.23 | 1.33 | 5.81× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.51 | 5.29× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.46 | 5.11× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.26 | 3.50× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.24 | 2.01× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.26 | 1.81× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.63× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.10 | 1.44× |
| `gauge_sum_bare` | 2 | 8 | 0.10 | 0.11 | 1.08× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.09 | 0.98× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.03 | 0.28× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.23× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.19× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.10 | 0.02 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.06 | 0.00 | 0.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.14 | 0.00 | 0.01× |
| `counter_ratio_generic` | 2 | 8 | 0.11 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.14 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns` | 1 | 4 | 0.69 | 18.52 | 26.88× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.44 | 11.50 | 26.19× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.88 | 20.62 | 23.32× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.30 | 5.01 | 16.62× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.45 | 5.89 | 13.00× |
| `memory_util_pct` | 1 | 4 | 0.19 | 1.96 | 10.55× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.10 | 10.34 | 9.37× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.17 | 1.54 | 9.26× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 1.10 | 9.96 | 9.09× |
| `counter_ratio_scaled` | 1 | 4 | 0.85 | 6.96 | 8.21× |
| `gauge_subtract` | 1 | 4 | 0.17 | 1.37 | 8.18× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.15 | 1.13 | 7.65× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 1.03 | 7.71 | 7.47× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.22 | 8.74 | 7.18× |
| `counter_total_sum_generic` | 11 | 44 | 0.36 | 2.54 | 7.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.24 | 1.70 | 6.98× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.45 | 10.01 | 6.92× |
| `counter_ratio_generic` | 2 | 8 | 0.90 | 5.82 | 6.47× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 1.06 | 6.71 | 6.33× |
| `gauge_bare` | 10 | 40 | 0.12 | 0.72 | 6.17× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 3.14 | 18.34 | 5.85× |
| `counter_irate_total_scaled` | 1 | 4 | 0.24 | 1.38 | 5.72× |
| `counter_irate_total_mul` | 2 | 8 | 0.21 | 1.18 | 5.58× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 4.06 | 22.16 | 5.45× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.18 | 0.94 | 5.22× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.47 | 2.41 | 5.10× |
| `gauge_max_bare` | 1 | 4 | 0.13 | 0.59 | 4.69× |
| `gauge_sum_scaled` | 1 | 4 | 0.15 | 0.68 | 4.47× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.67 | 23.93 | 4.22× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.50 | 9.76 | 3.91× |
| `softirq_irate_total_by_kind` | 10 | 40 | 1.18 | 4.40 | 3.74× |
| `counter_rate_bare_generic` | 2 | 8 | 0.70 | 2.50 | 3.58× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.48 | 1.68 | 3.48× |
| `gauge_sum_bare` | 2 | 8 | 0.14 | 0.29 | 2.05× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.10 | 0.20 | 1.95× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.12 | 0.20 | 1.65× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.22 | 1.56× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.16 | 0.24 | 1.47× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.39 | 0.57 | 1.47× |
| `gauge_sum_with_labels` | 5 | 20 | 0.14 | 0.17 | 1.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.12 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.01 | 0.08× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.09 | 0.01 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.06 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.13 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.09 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 4.06 | 17.39× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.21 | 3.45 | 16.46× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.39 | 6.03 | 15.37× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 1.24 | 15.36× |
| `counter_ratio_generic` | 2 | 8 | 0.29 | 4.36 | 15.29× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 1.16 | 14.95× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.86 | 12.22 | 14.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.41 | 5.62 | 13.60× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.47 | 6.37 | 13.51× |
| `counter_ratio_scaled` | 1 | 4 | 0.24 | 3.19 | 13.47× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.50 | 6.47 | 13.00× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 2.59 | 12.73× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 1.04 | 12.64× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.24 | 2.98 | 12.62× |
| `counter_total_sum_generic` | 11 | 44 | 0.15 | 1.76 | 12.11× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.75 | 11.10× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.65 | 10.82× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.26 | 2.75 | 10.60× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.79 | 8.37 | 10.59× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.79 | 9.86× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 4.28 | 9.34× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.19 | 1.70 | 9.07× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.78 | 8.25× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 0.93 | 7.08× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.49 | 6.33× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.43 | 5.89× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.39 | 5.79× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.41 | 4.98× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.33 | 4.28× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.21 | 0.46 | 2.14× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.11 | 1.90× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.19 | 1.78× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.11 | 1.78× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.16 | 0.25 | 1.60× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.09 | 1.31× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.08 | 1.28× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.11 | 1.18× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.09 | 1.18× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.07 | 1.06× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 0.12 | 0.90× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.15 | 0.09 | 0.64× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.21× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.42 | 6.42 | 15.26× |
| `counter_ratio_generic` | 2 | 8 | 0.36 | 4.99 | 14.02× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.27 | 3.50 | 13.03× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.25 | 3.00 | 12.11× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.13 | 11.51× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.24 | 2.65 | 11.04× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.58 | 6.07 | 10.41× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.53 | 5.52 | 10.37× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.84 | 8.42 | 9.97× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.17 | 1.74 | 9.96× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.12 | 1.14 | 9.86× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 1.00 | 9.79 | 9.83× |
| `counter_total_sum_generic` | 11 | 44 | 0.18 | 1.63 | 9.32× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.95 | 8.18× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.56 | 4.54 | 8.10× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.52 | 7.83× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.81 | 7.73× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.59 | 7.48× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.17 | 1.29 | 7.43× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.63 | 7.34× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.71 | 5.18 | 7.34× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.24 | 1.71 | 7.20× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.74 | 5.02 | 6.77× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 0.97 | 6.60× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.56 | 6.22× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.25 | 1.48 | 5.99× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.36 | 5.28× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.39 | 5.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.38 | 4.72× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.38 | 3.81× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.43 | 2.21× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.21 | 1.98× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.19 | 1.60× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.09 | 1.54× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.12 | 1.51× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.10 | 1.43× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.40× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.09 | 1.39× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.08 | 0.99× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.25× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.13 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.28 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.23 | 4.00 | 17.14× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.38 | 6.33 | 16.86× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.70 | 11.74 | 16.78× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 1.10 | 15.99× |
| `counter_ratio_generic` | 2 | 8 | 0.33 | 5.08 | 15.29× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.53 | 14.94× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 3.46 | 14.71× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 3.01 | 14.28× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.19 | 2.65 | 14.00× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.49 | 6.44 | 13.06× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.47 | 5.76 | 12.33× |
| `counter_total_sum_generic` | 11 | 44 | 0.16 | 2.03 | 12.33× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.57 | 6.86 | 12.03× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.71 | 11.87× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.18 | 2.09 | 11.75× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.72 | 8.29 | 11.51× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.76 | 10.74× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 1.11 | 10.56× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.77 | 9.82× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.08 | 9.08× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.26 | 2.26 | 8.81× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.76 | 8.56× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.09 | 0.73 | 8.28× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.33 | 2.65 | 8.14× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 1.00 | 7.91× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.18 | 1.26 | 7.12× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.59 | 4.10 | 7.00× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.41 | 6.04× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 0.64 | 5.97× |
| `gauge_avg_scaled` | 6 | 24 | 0.09 | 0.43 | 4.89× |
| `gauge_sum_scaled` | 1 | 4 | 0.11 | 0.44 | 3.88× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.38 | 3.79× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.25 | 2.32× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.11 | 1.95× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.25 | 0.47 | 1.90× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.22 | 1.90× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.12 | 1.59× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.56× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.11 | 1.41× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.09 | 1.39× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.11 | 1.15× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.08 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.49× | 0.10 | 1.79 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.92× | 0.05 | 0.77 | `gauge_subtract` | `memory_total - memory_available` |
| 16.67× | 0.10 | 1.63 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 15.66× | 0.05 | 0.81 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 14.41× | 0.07 | 1.08 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.89× | 0.11 | 1.51 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.16× | 0.21 | 2.83 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 13.03× | 0.13 | 1.74 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 12.99× | 0.18 | 2.37 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 12.89× | 0.12 | 1.58 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 16.90× | 0.10 | 1.74 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.58× | 0.05 | 0.76 | `gauge_subtract` | `memory_total - memory_available` |
| 16.27× | 0.09 | 1.51 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.85× | 0.05 | 0.75 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 13.55× | 0.11 | 1.53 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 13.26× | 0.07 | 0.91 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.60× | 0.11 | 1.37 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 12.46× | 0.12 | 1.50 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 12.36× | 0.12 | 1.54 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 12.19× | 0.10 | 1.24 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_upstream_responses_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_tokens_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.76× | 0.10 | 1.92 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.33× | 0.09 | 1.54 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 15.06× | 0.23 | 3.50 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 14.44× | 0.05 | 0.72 | `gauge_subtract` | `memory_total - memory_available` |
| 13.81× | 0.07 | 0.97 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.53× | 0.13 | 1.69 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.75× | 0.10 | 1.24 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 12.50× | 0.06 | 0.72 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.11× | 0.19 | 2.32 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 12.09× | 0.14 | 1.69 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.13 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang-decode"}` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.62× | 0.09 | 1.55 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 16.50× | 0.10 | 1.60 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.77× | 0.05 | 0.68 | `gauge_subtract` | `memory_total - memory_available` |
| 14.41× | 0.07 | 0.95 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 14.22× | 0.11 | 1.59 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.70× | 0.12 | 1.69 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.30× | 0.07 | 0.96 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.95× | 0.12 | 1.57 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 12.89× | 0.10 | 1.29 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 12.85× | 0.06 | 0.74 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 28.46× | 0.16 | 4.55 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 22.47× | 0.41 | 9.14 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 21.29× | 0.28 | 6.00 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 20.70× | 0.13 | 2.78 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 20.15× | 0.34 | 6.87 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 19.83× | 0.35 | 6.98 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 19.50× | 0.35 | 6.81 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 19.31× | 0.35 | 6.77 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 18.54× | 0.39 | 7.23 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 18.34× | 0.39 | 7.23 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.11 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.09 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.10 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.61× | 0.06 | 0.93 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.33× | 0.17 | 2.37 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 13.88× | 0.38 | 5.26 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 13.42× | 0.11 | 1.44 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.80× | 0.10 | 1.24 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 12.53× | 0.11 | 1.38 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 12.40× | 0.12 | 1.46 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 12.36× | 0.11 | 1.40 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 12.32× | 0.13 | 1.58 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 12.23× | 0.15 | 1.80 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.08 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 27.64× | 0.10 | 2.71 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 26.40× | 0.24 | 6.31 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 18.38× | 0.16 | 2.99 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 17.69× | 0.21 | 3.69 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.26× | 0.12 | 2.15 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 15.93× | 0.15 | 2.32 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 15.41× | 0.14 | 2.16 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 14.46× | 0.21 | 3.06 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 14.29× | 0.14 | 2.01 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 13.81× | 0.08 | 1.06 | `gauge_ratio_with_labels_ignoring` | `gpu_pcie_throughput{direction="receive"} / ignoring(direction) gpu_pcie_bandwidth` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_tpot_seconds{source="sglang-router"}` |
| 0.01× | 0.11 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.11 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 26.88× | 0.69 | 18.52 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 26.19× | 0.44 | 11.50 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 26.07× | 0.24 | 6.16 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 23.32× | 0.88 | 20.62 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.06× | 0.46 | 9.29 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 18.44× | 0.30 | 5.56 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 17.41× | 0.29 | 5.01 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 15.79× | 0.35 | 5.58 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 14.63× | 0.24 | 3.46 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 14.24× | 0.37 | 5.21 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.13 | 0.00 | `gauge_avg_scaled` | `avg(gpu_tensor_utilization) / 100` |
| 0.01× | 0.12 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.14 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_utilization) / 100` |
| 0.01× | 0.15 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.14 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.16 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_instructions[5m]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 20.92× | 0.19 | 3.87 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 20.83× | 0.21 | 4.29 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 20.17× | 0.38 | 7.58 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 19.25× | 0.19 | 3.62 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 18.56× | 0.35 | 6.47 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 18.53× | 0.21 | 3.92 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 18.34× | 0.24 | 4.36 | `counter_ratio_generic` | `sum(irate(cpu_branch_misses[5m])) / sum(irate(cpu_branch_instructions[5m]))` |
| 17.89× | 0.21 | 3.84 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 17.49× | 0.16 | 2.80 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="sched"}[5m]))` |
| 17.39× | 0.23 | 4.06 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.00× | 0.23 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.95× | 0.26 | 4.84 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 15.83× | 0.22 | 3.49 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 15.26× | 0.42 | 6.42 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 14.86× | 0.27 | 3.99 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 14.33× | 0.24 | 3.50 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 14.16× | 0.22 | 3.09 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 14.02× | 0.36 | 4.99 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 13.84× | 0.26 | 3.62 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 13.68× | 0.68 | 9.33 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 13.58× | 0.37 | 5.07 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.28 | 0.00 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 0.01× | 0.17 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="decode_forward",source="sglang-decode"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 20.99× | 0.25 | 5.33 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 20.20× | 0.20 | 4.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 19.54× | 0.22 | 4.32 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="local_mm_shootdown"}[5m]))` |
| 19.10× | 0.38 | 7.30 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 18.76× | 0.17 | 3.25 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 18.54× | 0.23 | 4.23 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 18.53× | 0.22 | 4.11 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 18.17× | 0.26 | 4.79 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 18.05× | 0.20 | 3.59 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 17.99× | 0.23 | 4.15 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |

