# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 314.5 ms | 23.0 ms | 8.0 ms | 239.2 ms | 1.6 ms | 231.8 ms |
| `AB_base_pin.parquet` | 3.1 | 326.3 ms | 9.2 ms | 3.2 ms | 230.1 ms | 0.1 ms | 227.5 ms |
| `AB_level.parquet` | 3.3 | 340.5 ms | 9.3 ms | 2.9 ms | 228.6 ms | 0.1 ms | 227.6 ms |
| `AB_level_pin.parquet` | 2.8 | 330.1 ms | 10.0 ms | 3.5 ms | 214.8 ms | 0.2 ms | 227.2 ms |
| `cachecannon.parquet` | 3.8 | 358.9 ms | 9.1 ms | 2.9 ms | 248.8 ms | 0.2 ms | 266.6 ms |
| `demo.parquet` | 1.2 | 109.1 ms | 10.8 ms | 3.5 ms | 101.2 ms | 0.2 ms | 115.5 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 730.9 ms | 8.8 ms | 3.8 ms | 1014.1 ms | 0.3 ms | 892.3 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2564.7 ms | 29.3 ms | 5.5 ms | 4138.9 ms | 0.9 ms | 3776.5 ms |
| `sglang_gemma3.parquet` | 2.1 | 344.4 ms | 22.4 ms | 2.9 ms | 510.7 ms | 0.6 ms | 501.6 ms |
| `vllm.parquet` | 4.0 | 375.9 ms | 20.7 ms | 4.3 ms | 466.0 ms | 0.7 ms | 478.3 ms |
| `vllm_gemma3.parquet` | 2.1 | 322.8 ms | 18.1 ms | 3.9 ms | 460.8 ms | 1.4 ms | 460.2 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.115 | 0.659 | 5.74× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.110 | 0.599 | 5.45× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.114 | 0.636 | 5.60× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.121 | 0.669 | 5.54× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.153 | 1.149 | 7.51× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.078 | 0.410 | 5.27× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.205 | 1.314 | 6.42× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.937 | 3.536 | 3.77× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.142 | 1.146 | 8.08× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.210 | 1.379 | 6.57× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.150 | 1.215 | 8.10× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.05 | 0.75 | 14.93× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.50 | 13.95× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 4.82 | 13.64× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.79 | 13.53× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.21 | 2.87 | 13.47× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.51 | 12.78× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.36 | 12.42× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.46 | 11.92× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.63 | 10.40× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.02 | 10.33× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.05 | 10.18× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.25 | 10.09× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.35 | 9.06× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.84 | 8.01× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.06 | 7.43× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 2.86 | 7.07× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.15 | 0.93 | 6.40× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.41 | 6.28× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.74 | 5.95× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.25 | 1.46 | 5.71× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.32 | 4.93× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.46 | 4.37× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.13 | 3.07× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.11 | 2.47× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.21 | 2.37× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.24 | 1.88× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.10 | 1.56× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.03 | 0.43× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.29× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.37 | 12.62× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.33 | 4.13 | 12.37× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 1.97 | 11.34× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.45 | 11.15× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.10 | 1.12 | 11.12× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.54 | 11.02× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.59 | 10.57× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 2.34 | 10.00× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.20 | 9.99× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.80 | 9.76× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 1.65 | 9.45× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.07 | 9.02× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.68 | 9.02× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.86 | 8.59× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.63 | 8.53× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.71 | 8.16× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.08 | 8.15× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.36 | 2.61 | 7.34× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.48 | 6.96× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.29 | 4.50× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.18 | 0.82 | 4.50× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.33 | 3.09× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.12 | 3.03× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.18 | 2.30× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.18 | 2.10× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.08 | 1.78× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.08 | 1.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.25× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.23× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.01 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.56 | 14.42× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.66 | 13.82× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.38 | 4.70 | 12.48× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.98 | 11.96× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.68 | 11.87× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 2.37 | 11.72× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.29 | 11.61× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 2.65 | 11.29× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.63 | 11.07× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.38 | 11.00× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 1.95 | 10.94× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.30 | 10.63× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.26 | 9.23× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.73 | 9.09× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.95 | 8.85× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.84 | 8.61× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.19 | 1.58 | 8.29× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.94 | 8.28× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.43 | 6.76× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 2.58 | 6.75× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.33 | 4.90× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.41 | 3.67× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.12 | 3.09× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.22 | 2.74× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.20 | 2.48× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.09 | 1.94× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.10 | 1.64× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.03 | 0.49× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.21× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.15 | 0.03 | 0.19× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.63 | 16.82× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.72 | 15.44× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.81 | 14.68× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.36 | 4.49 | 12.36× |
| `counter_ratio_scaled` | 1 | 4 | 0.22 | 2.55 | 11.33× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.66 | 11.12× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 2.72 | 10.93× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.22 | 2.35 | 10.75× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.02 | 10.47× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.12 | 10.23× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.01 | 10.06× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.07 | 9.34× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.82 | 9.06× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.08 | 0.74 | 8.87× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.26 | 8.59× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 1.36 | 7.68× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.09 | 7.67× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.42 | 6.63× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 2.66 | 6.26× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.20 | 1.00 | 4.91× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.34 | 4.88× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.14 | 0.42 | 2.90× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.12 | 2.80× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.11 | 2.77× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.23 | 2.56× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.24 | 2.38× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.11 | 1.84× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.35× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.20× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.01 | 0.18× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.17 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.36 | 8.86 | 24.50× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 2.67 | 16.24× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.70 | 10.27 | 14.76× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.25 | 3.62 | 14.46× |
| `counter_ratio_generic` | 2 | 8 | 0.56 | 7.85 | 14.01× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.89 | 13.05× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.32 | 4.01 | 12.73× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.31 | 3.93 | 12.70× |
| `counter_total_sum_generic` | 11 | 44 | 0.13 | 1.45 | 10.99× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.41 | 4.08 | 9.98× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.29 | 2.82 | 9.79× |
| `gauge_subtract` | 1 | 4 | 0.10 | 0.86 | 8.92× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.31 | 11.48 | 8.75× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.76 | 6.65 | 8.73× |
| `counter_ratio_scaled` | 1 | 4 | 0.59 | 4.78 | 8.11× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.74 | 8.07× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 1.01 | 7.97× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.10 | 0.71 | 7.17× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.14 | 6.89× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.40 | 4.71× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.24 | 3.94× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.41 | 3.05× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.10 | 1.98× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.22 | 1.97× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.25 | 1.86× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.11 | 1.14× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.08 | 0.63× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.21× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.20× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.19 | 0.02 | 0.11× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.22 | 0.02 | 0.08× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.38 | 5.43 | 14.33× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.17 | 2.38 | 13.59× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.64 | 12.81× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.46 | 12.52× |
| `counter_ratio_generic` | 2 | 8 | 0.15 | 1.90 | 12.45× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 0.86 | 12.13× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.92 | 11.77× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.42 | 11.64× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.67 | 10.97× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.47 | 9.98× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.90 | 9.49× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.34 | 9.37× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.58 | 8.92× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 3.08 | 6.96× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 0.93 | 6.60× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.07 | 0.43 | 6.05× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.40 | 3.80× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.20 | 2.80× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.23 | 2.73× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.10 | 2.71× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.05 | 0.09 | 1.79× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.07 | 1.70× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.07 | 1.19× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.42× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.40× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.14 | 0.03 | 0.24× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.21× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.20× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.22 | 6.20 | 28.14× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.94 | 14.19× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.21 | 2.77 | 13.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.35 | 12.30× |
| `memory_util_pct` | 1 | 4 | 0.08 | 1.02 | 12.08× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 2.04 | 11.96× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.21 | 2.41 | 11.53× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.25 | 2.70 | 10.69× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.32 | 3.33 | 10.46× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.28 | 2.90 | 10.42× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.46 | 4.70 | 10.28× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.85 | 9.41× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.29 | 2.66 | 9.23× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.11 | 0.94 | 8.95× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.55 | 4.85 | 8.83× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.16 | 1.42 | 8.77× |
| `counter_ratio_scaled` | 1 | 4 | 0.40 | 3.42 | 8.62× |
| `counter_rate_bare_generic` | 2 | 8 | 0.20 | 1.70 | 8.41× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 1.02 | 7.80× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.04 | 7.73 | 7.43× |
| `gauge_subtract` | 1 | 4 | 0.12 | 0.79 | 6.37× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.19 | 1.17 | 6.23× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.53 | 5.19× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.46 | 5.13× |
| `gauge_bare` | 10 | 40 | 0.08 | 0.33 | 3.90× |
| `gauge_max_bare` | 1 | 4 | 0.16 | 0.53 | 3.35× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.13 | 0.24 | 1.88× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.13 | 1.66× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.19 | 0.31 | 1.60× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.10 | 1.47× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.09 | 1.14× |
| `gauge_sum_bare` | 2 | 8 | 0.13 | 0.15 | 1.12× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.03 | 0.28× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.24× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.13 | 0.02 | 0.17× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.14 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.00 | 0.01× |
| `counter_ratio_generic` | 2 | 8 | 0.12 | 0.00 | 0.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.17 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns` | 1 | 4 | 0.54 | 18.68 | 34.34× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.43 | 11.96 | 27.88× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.76 | 17.93 | 23.48× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.25 | 5.18 | 20.68× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.44 | 4.78 | 10.98× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.18 | 1.79 | 9.72× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.94 | 8.76 | 9.34× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.14 | 1.29 | 8.90× |
| `gauge_subtract` | 1 | 4 | 0.18 | 1.54 | 8.67× |
| `memory_util_pct` | 1 | 4 | 0.19 | 1.54 | 8.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.77 | 6.19 | 8.01× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.29 | 2.23 | 7.72× |
| `counter_irate_total_scaled` | 1 | 4 | 0.21 | 1.52 | 7.40× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.98 | 6.96 | 7.11× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.05 | 7.38 | 7.05× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.21 | 1.35 | 6.57× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 2.85 | 18.49 | 6.49× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.75 | 4.79 | 6.43× |
| `counter_irate_total_mul` | 2 | 8 | 0.22 | 1.33 | 6.07× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.43 | 8.57 | 6.01× |
| `counter_total_sum_generic` | 11 | 44 | 0.35 | 2.12 | 5.99× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 3.75 | 21.82 | 5.82× |
| `counter_ratio_generic` | 2 | 8 | 0.88 | 4.95 | 5.62× |
| `gauge_bare` | 10 | 40 | 0.12 | 0.66 | 5.61× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.96 | 5.32 | 5.56× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.15 | 0.79 | 5.29× |
| `counter_ratio_scaled` | 1 | 4 | 0.87 | 4.41 | 5.05× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.56 | 4.98× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.21 | 21.48 | 4.12× |
| `gauge_sum_scaled` | 1 | 4 | 0.12 | 0.47 | 3.79× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.43 | 1.56 | 3.59× |
| `counter_rate_bare_generic` | 2 | 8 | 0.72 | 2.44 | 3.40× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.35 | 7.22 | 3.07× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.24 | 0.57 | 2.40× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.11 | 0.20 | 1.77× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.11 | 0.20 | 1.75× |
| `gauge_sum_bare` | 2 | 8 | 0.12 | 0.19 | 1.59× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.17 | 0.26 | 1.53× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.16 | 0.23 | 1.42× |
| `gauge_sum_with_labels` | 5 | 20 | 0.13 | 0.15 | 1.11× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.12 | 0.02 | 0.15× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.08 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.09 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.31 | 5.58 | 17.95× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.20 | 3.42 | 17.13× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.18 | 2.81 | 15.83× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.20 | 3.01 | 15.25× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 2.95 | 14.41× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.74 | 10.27 | 13.90× |
| `counter_ratio_generic` | 2 | 8 | 0.28 | 3.86 | 13.79× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 0.91 | 13.47× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.80 | 13.33× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.43 | 5.70 | 13.14× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.19 | 2.32 | 12.38× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 1.07 | 11.94× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 1.01 | 11.94× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.63 | 11.80× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.47 | 5.41 | 11.48× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.64 | 7.12 | 11.22× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.50 | 5.54 | 11.07× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.72 | 10.81× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.57 | 10.24× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.66 | 9.51× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.27 | 2.34 | 8.77× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 3.78 | 8.13× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 0.98 | 6.97× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.53 | 6.39× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 0.77 | 6.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.38 | 5.57× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.37 | 5.42× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.33 | 5.11× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.33 | 4.30× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.37 | 2.35× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.11 | 2.02× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 2.00× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.18 | 1.99× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.10 | 1.73× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.11 | 1.51× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.09 | 1.38× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.08 | 1.34× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.08 | 0.10 | 1.17× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.08 | 1.14× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.08 | 0.09 | 1.08× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 0.09 | 0.90× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.21× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.42 | 6.07 | 14.43× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.27 | 3.54 | 13.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.23 | 12.51× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.26 | 3.12 | 12.04× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.58 | 6.93 | 11.88× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.82 | 9.70 | 11.79× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.62 | 7.00 | 11.27× |
| `counter_ratio_generic` | 2 | 8 | 0.38 | 4.19 | 10.95× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.97 | 10.54 | 10.91× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.28 | 2.82 | 10.14× |
| `counter_total_sum_generic` | 11 | 44 | 0.19 | 1.97 | 10.14× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.76 | 9.87× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.48 | 4.12 | 8.50× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.22 | 1.87 | 8.43× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.13 | 1.06 | 8.30× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.75 | 6.06 | 8.06× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.65 | 7.98× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.64 | 7.85× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.11 | 0.82 | 7.71× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.88 | 7.49× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.76 | 5.36 | 7.03× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.19 | 1.30 | 7.01× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.21 | 1.43 | 6.87× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.13 | 6.71× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 0.94 | 6.35× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.18 | 1.13 | 6.14× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.42 | 5.65× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.42 | 5.64× |
| `gauge_avg_scaled` | 6 | 24 | 0.09 | 0.46 | 4.87× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.36 | 4.14× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.41 | 2.72× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.51 | 2.71× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.13 | 0.24 | 1.80× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.10 | 1.47× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.10 | 1.44× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.10 | 1.34× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.10 | 1.31× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.08 | 0.94× |
| `gauge_sum_bare` | 2 | 8 | 0.12 | 0.10 | 0.88× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.12 | 0.03 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.08 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.10 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.15 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.24 | 4.31 | 18.25× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.41 | 6.90 | 16.90× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 3.75 | 16.75× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 1.11 | 16.58× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.23 | 3.85 | 16.49× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.35 | 5.55 | 15.70× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.20 | 3.02 | 15.38× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.56 | 7.74 | 13.73× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 1.12 | 13.59× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.54 | 7.21 | 13.27× |
| `counter_ratio_scaled` | 1 | 4 | 0.22 | 2.61 | 12.12× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 2.45 | 12.05× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.69 | 11.96× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.49 | 5.85 | 11.89× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.76 | 8.92 | 11.67× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 2.05 | 11.37× |
| `counter_total_sum_generic` | 11 | 44 | 0.15 | 1.65 | 11.10× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 1.08 | 11.07× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.69 | 11.04× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.84 | 10.19× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.74 | 9.80× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.49 | 4.75 | 9.66× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.80 | 9.57× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.32 | 9.50× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.14 | 1.28 | 9.16× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 1.00 | 7.43× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.39 | 5.50× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.38 | 5.04× |
| `counter_irate_total_scaled` | 1 | 4 | 0.10 | 0.48 | 5.04× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.34 | 4.69× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.37 | 4.45× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.20 | 0.77 | 3.90× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.40 | 2.35× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.12 | 1.88× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.20 | 1.73× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.21 | 1.48× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.12 | 1.45× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.09 | 1.41× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.08 | 1.17× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.10 | 1.12× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.10 | 0.09 | 0.90× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
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
| 15.18× | 0.11 | 1.63 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.13× | 0.07 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.93× | 0.05 | 0.75 | `gauge_subtract` | `memory_total - memory_available` |
| 13.95× | 0.11 | 1.55 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.69× | 0.10 | 1.37 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.64× | 0.35 | 4.82 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 13.53× | 0.06 | 0.79 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 13.47× | 0.21 | 2.87 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 13.18× | 0.12 | 1.59 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 12.78× | 0.20 | 2.51 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare` | `tcp_packet_latency` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-prefill"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.03 | 0.00 | `gauge_max_bare` | `max(gpu_temperature)` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 12.73× | 0.09 | 1.21 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 12.62× | 0.11 | 1.37 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 12.42× | 0.06 | 0.78 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.37× | 0.33 | 4.13 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 12.19× | 0.11 | 1.39 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.02× | 0.12 | 1.40 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 11.83× | 0.18 | 2.11 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 11.71× | 0.11 | 1.32 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 11.34× | 0.17 | 1.97 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 11.30× | 0.53 | 6.01 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.81× | 0.11 | 1.60 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.73× | 0.06 | 0.95 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.82× | 0.05 | 0.66 | `gauge_subtract` | `memory_total - memory_available` |
| 13.52× | 0.14 | 1.89 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.13× | 0.10 | 1.28 | `counter_total_sum_generic` | `sum(irate(cpu_tlb_flush[5m]))` |
| 12.59× | 0.11 | 1.34 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 12.48× | 0.11 | 1.43 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.48× | 0.38 | 4.70 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 12.41× | 0.14 | 1.68 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 12.29× | 0.11 | 1.35 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="free"})` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.12× | 0.10 | 1.66 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.57× | 0.11 | 1.76 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 15.48× | 0.10 | 1.52 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 15.44× | 0.05 | 0.72 | `gauge_subtract` | `memory_total - memory_available` |
| 14.88× | 0.07 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.68× | 0.06 | 0.81 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 14.22× | 0.11 | 1.51 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.16× | 0.11 | 1.62 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 13.38× | 0.10 | 1.32 | `counter_total_sum_generic` | `sum(irate(cpu_tlb_flush[5m]))` |
| 13.05× | 0.12 | 1.52 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_upstream_responses_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="used"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 44.35× | 0.13 | 5.61 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 24.50× | 0.36 | 8.86 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 21.36× | 0.36 | 7.67 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 18.81× | 0.34 | 6.46 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 18.28× | 0.15 | 2.83 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 17.98× | 0.36 | 6.50 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 17.52× | 0.41 | 7.19 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 17.51× | 0.40 | 7.09 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 17.49× | 0.37 | 6.41 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 17.15× | 0.16 | 2.82 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.08 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.00× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.13 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.98× | 0.15 | 2.27 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 14.76× | 0.10 | 1.53 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.72× | 0.07 | 1.00 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.51× | 0.06 | 0.88 | `counter_irate_total_mul` | `sum(irate(network_bytes{direction="receive"}[5m])) * 8` |
| 14.33× | 0.38 | 5.43 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 13.59× | 0.17 | 2.38 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 13.58× | 0.12 | 1.70 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.31× | 0.07 | 0.94 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |
| 12.89× | 0.12 | 1.51 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 12.81× | 0.05 | 0.64 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 34.84× | 0.11 | 3.66 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 28.14× | 0.22 | 6.20 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 16.53× | 0.24 | 4.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.87× | 0.07 | 1.17 | `gauge_ratio_with_labels_ignoring` | `gpu_pcie_throughput{direction="receive"} / ignoring(direction) gpu_pcie_bandwidth` |
| 15.82× | 0.33 | 5.27 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 15.79× | 0.14 | 2.15 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 15.04× | 0.20 | 3.07 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 13.64× | 0.15 | 2.02 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 13.64× | 0.26 | 3.59 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 13.53× | 0.19 | 2.63 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="hrtimer"}[5m]))` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.11 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_occupancy) / 100` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.12 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.19 | 0.00 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.01× | 0.04 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 34.34× | 0.54 | 18.68 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 27.88× | 0.43 | 11.96 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 25.98× | 0.36 | 9.35 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 24.42× | 0.21 | 5.20 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 23.48× | 0.76 | 17.93 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 23.20× | 0.26 | 6.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 21.57× | 0.25 | 5.38 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 20.77× | 0.25 | 5.21 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 20.28× | 0.26 | 5.18 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 17.74× | 0.31 | 5.57 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.11 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.21 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cache_misses{source="cachecannon"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.07 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.15 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_instructions[5m]))` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 19.28× | 0.16 | 3.17 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 19.16× | 0.10 | 2.01 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 18.88× | 0.19 | 3.62 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 18.60× | 0.18 | 3.32 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 18.04× | 0.36 | 6.45 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 17.95× | 0.31 | 5.58 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 17.62× | 0.20 | 3.61 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 17.59× | 0.30 | 5.28 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 17.58× | 0.19 | 3.42 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="local_mm_shootdown"}[5m]))` |
| 17.52× | 0.19 | 3.34 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang"}[5s]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.93× | 0.21 | 3.42 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 15.81× | 0.22 | 3.40 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 15.28× | 0.27 | 4.08 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 14.93× | 0.23 | 3.50 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 14.87× | 0.24 | 3.61 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 14.46× | 0.22 | 3.23 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 14.43× | 0.42 | 6.07 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 14.16× | 0.24 | 3.34 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 14.11× | 0.27 | 3.84 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 13.94× | 0.22 | 3.12 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.20 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.15 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.15 | 0.00 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `get_latency{source="cachecannon"}` |
| 0.01× | 0.13 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 20.31× | 0.35 | 7.21 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 20.07× | 0.24 | 4.80 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 19.32× | 0.20 | 3.79 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 18.97× | 0.20 | 3.75 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 18.25× | 0.24 | 4.31 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 17.89× | 0.35 | 6.28 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 17.39× | 0.32 | 5.55 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 17.33× | 0.23 | 4.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 17.15× | 0.22 | 3.77 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.05× | 0.18 | 3.04 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |

