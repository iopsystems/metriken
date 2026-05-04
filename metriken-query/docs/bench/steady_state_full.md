# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 30 (first rep dropped from stats)
**Warm-up passes**: 5
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 357.7 ms | 27.7 ms | 7.3 ms | 268.1 ms | 1.5 ms | 282.5 ms |
| `AB_base_pin.parquet` | 3.1 | 344.6 ms | 8.9 ms | 3.1 ms | 250.9 ms | 0.1 ms | 314.9 ms |
| `AB_level.parquet` | 3.3 | 367.2 ms | 8.9 ms | 3.1 ms | 246.5 ms | 0.1 ms | 258.6 ms |
| `AB_level_pin.parquet` | 2.8 | 340.4 ms | 10.1 ms | 2.8 ms | 238.8 ms | 0.2 ms | 326.6 ms |
| `cachecannon.parquet` | 3.8 | 698.7 ms | 13.4 ms | 5.8 ms | 428.8 ms | 0.2 ms | 352.7 ms |
| `demo.parquet` | 1.2 | 133.9 ms | 9.5 ms | 3.6 ms | 122.5 ms | 0.2 ms | 121.8 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 770.6 ms | 10.4 ms | 3.2 ms | 1501.8 ms | 0.2 ms | 1121.9 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2808.5 ms | 31.9 ms | 3.4 ms | 5700.8 ms | 2.7 ms | 8233.0 ms |
| `sglang_gemma3.parquet` | 2.1 | 314.0 ms | 20.7 ms | 4.6 ms | 1027.4 ms | 0.4 ms | 1667.9 ms |
| `vllm.parquet` | 4.0 | 342.9 ms | 15.2 ms | 3.2 ms | 451.1 ms | 0.4 ms | 591.6 ms |
| `vllm_gemma3.parquet` | 2.1 | 343.1 ms | 12.7 ms | 3.4 ms | 490.1 ms | 0.4 ms | 455.0 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 258 | 0 | 5 | 50 | 0 | 0.590 | 1.824 | 3.09× |
| `AB_base_pin.parquet` | 258 | 0 | 5 | 50 | 0 | 0.608 | 1.770 | 2.91× |
| `AB_level.parquet` | 258 | 0 | 5 | 50 | 0 | 0.586 | 1.770 | 3.02× |
| `AB_level_pin.parquet` | 258 | 0 | 5 | 50 | 0 | 1.158 | 4.014 | 3.46× |
| `cachecannon.parquet` | 254 | 0 | 5 | 54 | 0 | 0.741 | 1.992 | 2.69× |
| `demo.parquet` | 263 | 0 | 5 | 45 | 0 | 0.482 | 1.271 | 2.64× |
| `disagg/disagg-sglang.parquet` | 241 | 0 | 5 | 67 | 0 | 0.707 | 2.068 | 2.92× |
| `disagg/sglang-nixl-16c.parquet` | 227 | 0 | 5 | 81 | 0 | 1.649 | 3.724 | 2.26× |
| `sglang_gemma3.parquet` | 247 | 0 | 5 | 61 | 0 | 0.629 | 2.323 | 3.70× |
| `vllm.parquet` | 245 | 0 | 5 | 63 | 0 | 0.933 | 3.075 | 3.30× |
| `vllm_gemma3.parquet` | 245 | 0 | 5 | 63 | 0 | 0.693 | 2.502 | 3.61× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 58 | 0.09 | 0.64 | 6.89× |
| `counter_total_sum_generic` | 11 | 319 | 0.08 | 0.39 | 4.75× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 58 | 0.42 | 1.92 | 4.55× |
| `counter_irate_sum_with_labels` | 84 | 2436 | 0.06 | 0.26 | 4.46× |
| `counter_irate_total_mul` | 2 | 58 | 0.06 | 0.28 | 4.31× |
| `counter_irate_total_scaled` | 1 | 29 | 0.07 | 0.30 | 4.24× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.10 | 0.42 | 4.21× |
| `counter_ratio_generic` | 2 | 58 | 0.16 | 0.63 | 3.81× |
| `counter_ratio_scaled` | 1 | 29 | 0.17 | 0.64 | 3.80× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.12 | 0.45 | 3.67× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.21 | 0.73 | 3.54× |
| `gauge_bare` | 5 | 145 | 0.04 | 0.11 | 2.74× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.13 | 0.31 | 2.51× |
| `gauge_subtract` | 1 | 29 | 0.05 | 0.11 | 2.45× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.03 | 0.08 | 2.44× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.03 | 0.07 | 2.34× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.03 | 0.07 | 2.31× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.03 | 0.07 | 2.29× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.04 | 0.08 | 2.29× |
| `counter_irate_by_id_with_labels` | 4 | 116 | 0.03 | 0.07 | 2.25× |
| `gauge_avg_scaled` | 6 | 174 | 0.04 | 0.07 | 2.04× |
| `gauge_bare_with_labels` | 33 | 957 | 0.03 | 0.07 | 2.03× |
| `gauge_sum_with_labels` | 5 | 145 | 0.04 | 0.07 | 1.97× |
| `memory_util_pct` | 1 | 29 | 0.07 | 0.13 | 1.97× |
| `gauge_sum_scaled` | 1 | 29 | 0.03 | 0.06 | 1.93× |
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.04 | 0.08 | 1.87× |
| `gauge_max_bare` | 1 | 29 | 0.04 | 0.07 | 1.86× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.04 | 0.08 | 1.84× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.04 | 0.07 | 1.81× |
| `softirq_time_pct_by_id_by_kind` | 5 | 145 | 0.05 | 0.08 | 1.77× |
| `gauge_sum_bare` | 2 | 58 | 0.05 | 0.08 | 1.70× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.05 | 0.08 | 1.66× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.04 | 0.07 | 1.65× |
| `softirq_irate_by_id_by_kind` | 5 | 145 | 0.05 | 0.08 | 1.65× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.04 | 0.07 | 1.62× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.05 | 0.07 | 1.56× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.05 | 0.08 | 1.56× |
| `counter_ratio_complement` | 1 | 29 | 0.05 | 0.07 | 1.54× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.05 | 0.08 | 1.49× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.05 | 0.07 | 1.38× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.08 | 1.24× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.06 | 0.07 | 1.23× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.10 | 0.09 | 0.91× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.07 | 0.05 | 0.77× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.08 | 0.05 | 0.65× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 58 | 0.10 | 0.61 | 6.27× |
| `counter_total_sum_generic` | 11 | 319 | 0.08 | 0.37 | 4.72× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.17 | 0.77 | 4.38× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 58 | 0.38 | 1.60 | 4.22× |
| `counter_ratio_scaled` | 1 | 29 | 0.16 | 0.66 | 4.21× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.08 | 0.35 | 4.19× |
| `counter_irate_sum_with_labels` | 84 | 2436 | 0.06 | 0.23 | 3.83× |
| `counter_ratio_generic` | 2 | 58 | 0.17 | 0.66 | 3.77× |
| `counter_irate_total_scaled` | 1 | 29 | 0.08 | 0.29 | 3.74× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.12 | 0.44 | 3.63× |
| `counter_irate_total_mul` | 2 | 58 | 0.07 | 0.23 | 3.56× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.11 | 0.30 | 2.84× |
| `gauge_bare` | 5 | 145 | 0.04 | 0.12 | 2.69× |
| `gauge_subtract` | 1 | 29 | 0.05 | 0.11 | 2.41× |
| `memory_util_pct` | 1 | 29 | 0.07 | 0.14 | 2.01× |
| `gauge_max_bare` | 1 | 29 | 0.04 | 0.07 | 1.95× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.04 | 0.08 | 1.87× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.04 | 0.08 | 1.73× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.05 | 0.08 | 1.68× |
| `gauge_bare_with_labels` | 33 | 957 | 0.04 | 0.07 | 1.64× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.08 | 1.60× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.05 | 0.07 | 1.59× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.05 | 0.07 | 1.58× |
| `gauge_sum_scaled` | 1 | 29 | 0.04 | 0.07 | 1.57× |
| `gauge_sum_with_labels` | 5 | 145 | 0.05 | 0.07 | 1.57× |
| `softirq_irate_by_id_by_kind` | 5 | 145 | 0.05 | 0.08 | 1.56× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.05 | 0.07 | 1.55× |
| `gauge_avg_scaled` | 6 | 174 | 0.05 | 0.07 | 1.53× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.04 | 0.07 | 1.52× |
| `softirq_time_pct_by_id_by_kind` | 5 | 145 | 0.05 | 0.08 | 1.52× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.05 | 0.07 | 1.50× |
| `counter_irate_by_id_with_labels` | 4 | 116 | 0.05 | 0.07 | 1.48× |
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.05 | 0.08 | 1.46× |
| `counter_ratio_complement` | 1 | 29 | 0.05 | 0.08 | 1.46× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.05 | 0.08 | 1.45× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.05 | 0.07 | 1.36× |
| `gauge_sum_bare` | 2 | 58 | 0.06 | 0.08 | 1.34× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.05 | 0.06 | 1.23× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.05 | 0.06 | 1.22× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.07 | 0.08 | 1.19× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.08 | 1.17× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.07 | 0.08 | 1.14× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.07 | 0.06 | 0.90× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.10 | 0.09 | 0.86× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.09 | 0.05 | 0.60× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 58 | 0.09 | 0.64 | 6.79× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 58 | 0.39 | 1.77 | 4.51× |
| `counter_total_sum_generic` | 11 | 319 | 0.08 | 0.36 | 4.46× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.10 | 0.42 | 4.42× |
| `counter_irate_sum_with_labels` | 84 | 2436 | 0.06 | 0.25 | 4.35× |
| `counter_irate_total_mul` | 2 | 58 | 0.07 | 0.28 | 4.07× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.16 | 0.63 | 3.95× |
| `counter_ratio_scaled` | 1 | 29 | 0.17 | 0.65 | 3.88× |
| `counter_ratio_generic` | 2 | 58 | 0.16 | 0.63 | 3.85× |
| `counter_irate_total_scaled` | 1 | 29 | 0.07 | 0.28 | 3.76× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.12 | 0.43 | 3.65× |
| `gauge_bare` | 5 | 145 | 0.04 | 0.11 | 2.77× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.11 | 0.29 | 2.75× |
| `gauge_subtract` | 1 | 29 | 0.04 | 0.11 | 2.45× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.04 | 0.08 | 2.17× |
| `memory_util_pct` | 1 | 29 | 0.06 | 0.13 | 2.09× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.04 | 0.08 | 1.99× |
| `gauge_sum_with_labels` | 5 | 145 | 0.04 | 0.08 | 1.90× |
| `gauge_sum_scaled` | 1 | 29 | 0.04 | 0.08 | 1.85× |
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.04 | 0.08 | 1.85× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.05 | 0.08 | 1.83× |
| `softirq_time_pct_by_id_by_kind` | 5 | 145 | 0.05 | 0.08 | 1.77× |
| `gauge_avg_scaled` | 6 | 174 | 0.04 | 0.08 | 1.77× |
| `gauge_bare_with_labels` | 33 | 957 | 0.04 | 0.07 | 1.77× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.04 | 0.08 | 1.75× |
| `softirq_irate_by_id_by_kind` | 5 | 145 | 0.05 | 0.09 | 1.74× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.05 | 0.08 | 1.74× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.04 | 0.08 | 1.73× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.05 | 0.08 | 1.72× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.05 | 0.08 | 1.72× |
| `gauge_max_bare` | 1 | 29 | 0.04 | 0.07 | 1.69× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.05 | 0.08 | 1.66× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.05 | 0.07 | 1.64× |
| `counter_ratio_complement` | 1 | 29 | 0.05 | 0.08 | 1.62× |
| `counter_irate_by_id_with_labels` | 4 | 116 | 0.05 | 0.08 | 1.58× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.05 | 0.08 | 1.58× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.05 | 0.07 | 1.50× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.05 | 0.08 | 1.48× |
| `gauge_sum_bare` | 2 | 58 | 0.06 | 0.08 | 1.44× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.06 | 0.08 | 1.41× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.09 | 1.28× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.06 | 0.08 | 1.24× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.10 | 0.09 | 0.98× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.07 | 0.05 | 0.83× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.09 | 0.05 | 0.62× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 58 | 0.12 | 0.92 | 7.88× |
| `counter_total_sum_generic` | 11 | 319 | 0.10 | 0.67 | 6.84× |
| `counter_ratio_generic` | 2 | 58 | 0.21 | 1.03 | 4.95× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 58 | 0.54 | 2.58 | 4.79× |
| `counter_ratio_scaled` | 1 | 29 | 0.22 | 1.07 | 4.79× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.13 | 0.60 | 4.72× |
| `counter_irate_sum_with_labels` | 84 | 2436 | 0.07 | 0.33 | 4.45× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.25 | 1.07 | 4.34× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.16 | 0.68 | 4.21× |
| `counter_irate_total_scaled` | 1 | 29 | 0.08 | 0.35 | 4.16× |
| `counter_irate_total_mul` | 2 | 58 | 0.09 | 0.32 | 3.52× |
| `gauge_bare` | 5 | 145 | 0.06 | 0.15 | 2.73× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.20 | 0.50 | 2.51× |
| `memory_util_pct` | 1 | 29 | 0.07 | 0.17 | 2.41× |
| `gauge_subtract` | 1 | 29 | 0.06 | 0.14 | 2.14× |
| `gauge_sum_scaled` | 1 | 29 | 0.04 | 0.07 | 1.91× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.04 | 0.08 | 1.91× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.04 | 0.07 | 1.89× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.05 | 0.08 | 1.77× |
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.05 | 0.09 | 1.74× |
| `gauge_sum_with_labels` | 5 | 145 | 0.04 | 0.07 | 1.66× |
| `gauge_sum_bare` | 2 | 58 | 0.06 | 0.09 | 1.64× |
| `gauge_bare_with_labels` | 33 | 957 | 0.05 | 0.07 | 1.61× |
| `counter_irate_by_id_with_labels` | 4 | 116 | 0.05 | 0.07 | 1.58× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.05 | 0.08 | 1.56× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.04 | 0.06 | 1.54× |
| `gauge_max_bare` | 1 | 29 | 0.05 | 0.07 | 1.54× |
| `gauge_avg_scaled` | 6 | 174 | 0.05 | 0.07 | 1.51× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.06 | 0.08 | 1.50× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.05 | 0.08 | 1.44× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.05 | 0.07 | 1.40× |
| `softirq_time_pct_by_id_by_kind` | 5 | 145 | 0.06 | 0.08 | 1.38× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.05 | 0.07 | 1.36× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.04 | 0.06 | 1.32× |
| `softirq_irate_by_id_by_kind` | 5 | 145 | 0.07 | 0.09 | 1.26× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.06 | 0.08 | 1.25× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.06 | 0.07 | 1.24× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.07 | 0.08 | 1.18× |
| `counter_ratio_complement` | 1 | 29 | 0.06 | 0.07 | 1.18× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.08 | 0.10 | 1.16× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.06 | 0.07 | 1.15× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.07 | 0.08 | 1.08× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.11 | 0.09 | 0.75× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.16 | 0.11 | 0.69× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.10 | 0.07 | 0.67× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 58 | 0.13 | 0.92 | 6.98× |
| `counter_total_sum_generic` | 10 | 290 | 0.10 | 0.71 | 6.77× |
| `counter_ratio_scaled` | 1 | 29 | 0.58 | 3.65 | 6.32× |
| `counter_ratio_generic` | 2 | 58 | 0.59 | 3.61 | 6.13× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.22 | 1.22 | 5.52× |
| `counter_irate_total_scaled` | 1 | 29 | 0.09 | 0.41 | 4.65× |
| `counter_irate_total_mul` | 2 | 58 | 0.08 | 0.35 | 4.50× |
| `gauge_bare` | 5 | 145 | 0.06 | 0.28 | 4.28× |
| `counter_irate_sum_with_labels` | 85 | 2465 | 0.08 | 0.27 | 3.65× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.11 | 0.38 | 3.49× |
| `gauge_subtract` | 1 | 29 | 0.08 | 0.18 | 2.33× |
| `memory_util_pct` | 1 | 29 | 0.09 | 0.21 | 2.30× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.05 | 0.08 | 1.46× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.06 | 0.09 | 1.44× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.05 | 0.07 | 1.33× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.05 | 0.07 | 1.32× |
| `softirq_irate_by_id_by_kind` | 4 | 116 | 0.06 | 0.08 | 1.29× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.05 | 0.06 | 1.26× |
| `counter_irate_by_id_with_labels` | 4 | 116 | 0.05 | 0.07 | 1.25× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.05 | 0.07 | 1.24× |
| `gauge_sum_bare` | 2 | 58 | 0.06 | 0.08 | 1.23× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.06 | 0.07 | 1.20× |
| `softirq_time_pct_by_id_by_kind` | 4 | 116 | 0.06 | 0.08 | 1.18× |
| `gauge_bare_with_labels` | 33 | 957 | 0.05 | 0.06 | 1.15× |
| `gauge_avg_scaled` | 6 | 174 | 0.04 | 0.05 | 1.15× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.06 | 0.07 | 1.12× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.07 | 0.08 | 1.10× |
| `gauge_sum_with_labels` | 5 | 145 | 0.05 | 0.05 | 1.08× |
| `gauge_max_bare` | 1 | 29 | 0.04 | 0.05 | 1.08× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.07 | 0.08 | 1.06× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.07 | 1.04× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.06 | 1.03× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.06 | 0.06 | 1.03× |
| `counter_ratio_complement` | 1 | 29 | 0.06 | 0.06 | 1.02× |
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.05 | 0.05 | 1.02× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.05 | 0.05 | 1.01× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.06 | 0.06 | 1.00× |
| `gauge_sum_scaled` | 1 | 29 | 0.05 | 0.05 | 0.99× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.05 | 0.05 | 0.98× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.17 | 0.14 | 0.86× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.08 | 0.06 | 0.71× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.09 | 0.05 | 0.57× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.18 | 0.10 | 0.54× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.13 | 0.07 | 0.54× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 58 | 0.10 | 0.61 | 6.21× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 58 | 0.42 | 2.17 | 5.14× |
| `counter_ratio_generic` | 2 | 58 | 0.17 | 0.75 | 4.48× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.10 | 0.45 | 4.46× |
| `counter_irate_total_scaled` | 1 | 29 | 0.07 | 0.30 | 4.44× |
| `counter_total_sum_generic` | 11 | 319 | 0.06 | 0.26 | 4.29× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.11 | 0.46 | 4.21× |
| `counter_irate_total_mul` | 2 | 58 | 0.07 | 0.27 | 4.05× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.09 | 0.29 | 3.24× |
| `gauge_sum_scaled` | 1 | 29 | 0.03 | 0.07 | 2.01× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.04 | 0.07 | 1.76× |
| `gauge_bare` | 5 | 145 | 0.05 | 0.08 | 1.61× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.03 | 0.05 | 1.55× |
| `counter_irate_by_id_generic` | 4 | 116 | 0.03 | 0.05 | 1.53× |
| `gauge_bare_with_labels` | 36 | 1044 | 0.03 | 0.05 | 1.48× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.03 | 0.05 | 1.47× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.05 | 0.08 | 1.45× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.03 | 0.05 | 1.43× |
| `gauge_sum_with_labels` | 5 | 145 | 0.03 | 0.05 | 1.40× |
| `gauge_avg_scaled` | 6 | 174 | 0.03 | 0.04 | 1.39× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.06 | 0.08 | 1.38× |
| `counter_irate_sum_with_labels` | 85 | 2465 | 0.06 | 0.08 | 1.36× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.05 | 0.06 | 1.36× |
| `memory_util_pct` | 1 | 29 | 0.06 | 0.08 | 1.36× |
| `counter_irate_by_id_with_labels` | 4 | 116 | 0.03 | 0.05 | 1.34× |
| `gauge_sum_bare` | 2 | 58 | 0.06 | 0.07 | 1.34× |
| `softirq_time_pct_by_id_by_kind` | 4 | 116 | 0.05 | 0.06 | 1.32× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.04 | 0.06 | 1.31× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.04 | 0.05 | 1.30× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.03 | 0.04 | 1.27× |
| `gauge_subtract` | 1 | 29 | 0.06 | 0.07 | 1.27× |
| `softirq_irate_by_id_by_kind` | 4 | 116 | 0.05 | 0.06 | 1.27× |
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.04 | 0.05 | 1.27× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.04 | 0.05 | 1.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.05 | 0.06 | 1.24× |
| `counter_ratio_scaled` | 1 | 29 | 0.04 | 0.05 | 1.22× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.06 | 1.20× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.04 | 0.05 | 1.20× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.06 | 0.07 | 1.17× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.06 | 0.07 | 1.14× |
| `gauge_max_bare` | 1 | 29 | 0.04 | 0.04 | 1.08× |
| `counter_ratio_complement` | 1 | 29 | 0.04 | 0.04 | 1.07× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.09 | 0.08 | 0.85× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.07 | 0.06 | 0.83× |
| `counter_ratio_by_id_scaled` | 1 | 29 | 0.06 | 0.04 | 0.79× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.07 | 0.05 | 0.64× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.09 | 0.79 | 8.99× |
| `gauge_max_bare` | 1 | 29 | 0.06 | 0.43 | 7.33× |
| `counter_rate_bare_generic` | 2 | 58 | 0.16 | 0.85 | 5.27× |
| `counter_total_sum_generic` | 9 | 261 | 0.11 | 0.51 | 4.62× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.16 | 0.75 | 4.60× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.33 | 1.47 | 4.43× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.19 | 0.81 | 4.26× |
| `counter_irate_total_scaled` | 1 | 29 | 0.10 | 0.39 | 3.86× |
| `gauge_bare` | 5 | 145 | 0.06 | 0.23 | 3.78× |
| `counter_ratio_scaled` | 1 | 29 | 0.32 | 1.21 | 3.77× |
| `counter_irate_total_mul` | 2 | 58 | 0.09 | 0.33 | 3.74× |
| `counter_irate_sum_with_labels` | 83 | 2407 | 0.07 | 0.27 | 3.65× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.09 | 0.29 | 3.23× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.09 | 0.26 | 2.91× |
| `memory_util_pct` | 1 | 29 | 0.09 | 0.20 | 2.32× |
| `gauge_subtract` | 1 | 29 | 0.07 | 0.17 | 2.25× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 29 | 0.04 | 0.07 | 1.62× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.07 | 1.52× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.06 | 0.09 | 1.51× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.05 | 0.07 | 1.47× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.05 | 0.07 | 1.43× |
| `gauge_avg_scaled` | 6 | 174 | 0.05 | 0.07 | 1.36× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.05 | 0.07 | 1.36× |
| `gauge_bare_with_labels` | 27 | 783 | 0.05 | 0.07 | 1.33× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.07 | 0.09 | 1.33× |
| `softirq_irate_by_id_by_kind` | 3 | 87 | 0.06 | 0.08 | 1.29× |
| `softirq_time_pct_by_id_by_kind` | 3 | 87 | 0.06 | 0.08 | 1.28× |
| `counter_ratio_complement` | 1 | 29 | 0.05 | 0.07 | 1.27× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.05 | 0.07 | 1.26× |
| `gauge_sum_bare` | 2 | 58 | 0.07 | 0.09 | 1.19× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.06 | 0.07 | 1.17× |
| `counter_ratio_generic` | 2 | 58 | 0.07 | 0.08 | 1.16× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.07 | 0.08 | 1.11× |
| `gauge_sum_scaled` | 1 | 29 | 0.06 | 0.07 | 1.11× |
| `gauge_sum_with_labels` | 5 | 145 | 0.06 | 0.07 | 1.08× |
| `counter_ratio_by_id_generic` | 2 | 58 | 0.07 | 0.08 | 1.07× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.07 | 0.08 | 1.03× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.08 | 0.07 | 0.90× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.07 | 0.89× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.08 | 0.07 | 0.86× |
| `rezolus_cpu_ipns_per_id` | 1 | 29 | 0.07 | 0.05 | 0.72× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.12 | 0.08 | 0.68× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.08 | 0.05 | 0.63× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 29 | 0.09 | 0.04 | 0.44× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.15 | 1.07 | 6.98× |
| `gauge_max_bare` | 1 | 29 | 0.09 | 0.59 | 6.59× |
| `gauge_bare` | 5 | 145 | 0.11 | 0.57 | 5.34× |
| `counter_irate_sum_with_labels` | 73 | 2117 | 0.12 | 0.43 | 3.48× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.14 | 0.49 | 3.42× |
| `counter_irate_total_scaled` | 1 | 29 | 0.19 | 0.65 | 3.36× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.12 | 0.40 | 3.25× |
| `counter_irate_total_mul` | 2 | 58 | 0.17 | 0.55 | 3.25× |
| `counter_total_sum_generic` | 9 | 261 | 0.30 | 0.96 | 3.25× |
| `gauge_subtract` | 1 | 29 | 0.14 | 0.42 | 2.97× |
| `counter_rate_bare_generic` | 2 | 58 | 0.66 | 1.93 | 2.93× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.25 | 0.68 | 2.72× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.45 | 1.23 | 2.71× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.67 | 1.80 | 2.70× |
| `memory_util_pct` | 1 | 29 | 0.20 | 0.53 | 2.60× |
| `counter_ratio_scaled` | 1 | 29 | 0.93 | 1.95 | 2.09× |
| `counter_ratio_generic` | 2 | 58 | 0.96 | 1.97 | 2.06× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.06 | 0.08 | 1.46× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.08 | 1.43× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.09 | 0.13 | 1.37× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.09 | 0.12 | 1.35× |
| `softirq_time_pct_by_id_by_kind` | 4 | 116 | 0.11 | 0.14 | 1.30× |
| `counter_irate_by_id_generic` | 2 | 58 | 0.06 | 0.07 | 1.27× |
| `gauge_avg_scaled` | 6 | 174 | 0.07 | 0.09 | 1.26× |
| `counter_ratio_complement` | 1 | 29 | 0.07 | 0.08 | 1.26× |
| `counter_ratio_by_id_generic` | 1 | 29 | 0.07 | 0.08 | 1.19× |
| `gauge_bare_with_labels` | 28 | 812 | 0.06 | 0.07 | 1.18× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.07 | 0.08 | 1.17× |
| `softirq_irate_by_id_by_kind` | 4 | 116 | 0.12 | 0.14 | 1.15× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.08 | 0.09 | 1.12× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.06 | 0.07 | 1.08× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.11 | 0.11 | 1.01× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.10 | 0.10 | 0.97× |
| `gauge_sum_scaled` | 1 | 29 | 0.09 | 0.09 | 0.95× |
| `gauge_sum_with_labels` | 5 | 145 | 0.10 | 0.09 | 0.92× |
| `gauge_sum_bare` | 2 | 58 | 0.14 | 0.13 | 0.90× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.17 | 0.12 | 0.68× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.10 | 0.06 | 0.63× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.24 | 0.15 | 0.62× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.24 | 0.15 | 0.61× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.07 | 0.85 | 11.60× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.54 | 5.08 | 9.41× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.36 | 3.21 | 8.89× |
| `counter_rate_bare_generic` | 2 | 58 | 0.07 | 0.59 | 8.12× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.07 | 0.59 | 7.99× |
| `gauge_max_bare` | 1 | 29 | 0.05 | 0.36 | 7.19× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 29 | 0.27 | 1.65 | 6.12× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.14 | 0.87 | 6.08× |
| `gauge_avg_scaled` | 6 | 174 | 0.07 | 0.40 | 5.31× |
| `counter_ratio_scaled` | 1 | 29 | 0.18 | 0.95 | 5.31× |
| `counter_ratio_generic` | 2 | 58 | 0.24 | 1.29 | 5.26× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.19 | 0.91 | 4.79× |
| `counter_total_sum_generic` | 9 | 261 | 0.11 | 0.45 | 4.00× |
| `counter_irate_total_scaled` | 1 | 29 | 0.07 | 0.27 | 3.96× |
| `counter_irate_total_mul` | 2 | 58 | 0.07 | 0.29 | 3.95× |
| `counter_irate_sum_with_labels` | 84 | 2436 | 0.06 | 0.25 | 3.82× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.08 | 0.29 | 3.70× |
| `gauge_bare` | 5 | 145 | 0.05 | 0.10 | 2.00× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.05 | 0.11 | 1.98× |
| `gauge_subtract` | 1 | 29 | 0.05 | 0.10 | 1.94× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.08 | 0.15 | 1.92× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.06 | 0.11 | 1.89× |
| `memory_util_pct` | 1 | 29 | 0.07 | 0.13 | 1.88× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.09 | 1.86× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.05 | 0.10 | 1.84× |
| `gauge_bare_with_labels` | 32 | 928 | 0.04 | 0.07 | 1.72× |
| `gauge_sum_scaled` | 1 | 29 | 0.05 | 0.09 | 1.66× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.05 | 0.08 | 1.61× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.05 | 0.08 | 1.55× |
| `softirq_irate_by_id_by_kind` | 6 | 174 | 0.06 | 0.09 | 1.54× |
| `softirq_time_pct_by_id_by_kind` | 6 | 174 | 0.06 | 0.09 | 1.52× |
| `gauge_sum_with_labels` | 5 | 145 | 0.06 | 0.09 | 1.51× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.07 | 0.10 | 1.45× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.06 | 0.08 | 1.43× |
| `counter_ratio_complement` | 1 | 29 | 0.05 | 0.06 | 1.31× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.06 | 0.07 | 1.31× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.08 | 1.21× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.06 | 0.08 | 1.18× |
| `gauge_sum_bare` | 2 | 58 | 0.08 | 0.09 | 1.08× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.12 | 0.10 | 0.86× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.09 | 0.95 | 11.09× |
| `gauge_max_bare` | 1 | 29 | 0.07 | 0.45 | 6.72× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.50 | 3.35 | 6.66× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.88 | 5.76 | 6.53× |
| `counter_rate_bare_generic` | 2 | 58 | 0.11 | 0.72 | 6.30× |
| `counter_total_sum_generic` | 9 | 261 | 0.09 | 0.54 | 5.97× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.12 | 0.72 | 5.97× |
| `gauge_avg_scaled` | 6 | 174 | 0.09 | 0.45 | 4.80× |
| `counter_irate_with_labels_scaled` | 2 | 58 | 0.41 | 1.88 | 4.63× |
| `counter_irate_total_scaled` | 1 | 29 | 0.08 | 0.38 | 4.62× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 29 | 0.40 | 1.72 | 4.33× |
| `counter_ratio_generic` | 2 | 58 | 0.33 | 1.42 | 4.33× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.21 | 0.90 | 4.21× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.12 | 0.52 | 4.17× |
| `counter_irate_sum_with_labels` | 83 | 2407 | 0.08 | 0.27 | 3.61× |
| `counter_irate_total_mul` | 2 | 58 | 0.10 | 0.33 | 3.39× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.14 | 0.42 | 3.08× |
| `gauge_bare` | 5 | 145 | 0.06 | 0.10 | 1.82× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.09 | 0.17 | 1.76× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.06 | 0.09 | 1.60× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.06 | 0.10 | 1.59× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.06 | 0.10 | 1.56× |
| `gauge_subtract` | 1 | 29 | 0.06 | 0.09 | 1.55× |
| `memory_util_pct` | 1 | 29 | 0.08 | 0.11 | 1.50× |
| `counter_irate_by_id_generic` | 1 | 29 | 0.05 | 0.08 | 1.49× |
| `gauge_bare_with_labels` | 32 | 928 | 0.06 | 0.08 | 1.43× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.07 | 1.37× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.06 | 0.08 | 1.37× |
| `counter_ratio_complement` | 1 | 29 | 0.06 | 0.08 | 1.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.06 | 0.07 | 1.25× |
| `softirq_time_pct_by_id_by_kind` | 5 | 145 | 0.07 | 0.09 | 1.21× |
| `gauge_sum_scaled` | 1 | 29 | 0.06 | 0.08 | 1.20× |
| `gauge_sum_with_labels` | 5 | 145 | 0.07 | 0.08 | 1.20× |
| `softirq_irate_by_id_by_kind` | 5 | 145 | 0.08 | 0.09 | 1.12× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.06 | 0.06 | 1.08× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.07 | 0.08 | 1.04× |
| `gauge_sum_bare` | 2 | 58 | 0.10 | 0.09 | 0.97× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.09 | 0.08 | 0.94× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.09 | 0.08 | 0.83× |
| `counter_ratio_scaled` | 1 | 29 | 0.10 | 0.08 | 0.82× |
| `counter_ratio_by_id_scaled` | 1 | 29 | 0.10 | 0.07 | 0.73× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.15 | 0.10 | 0.69× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 29 | 0.08 | 0.88 | 11.78× |
| `rezolus_cpu_ipns` | 1 | 29 | 0.57 | 5.80 | 10.14× |
| `counter_rate_bare_generic` | 2 | 58 | 0.08 | 0.64 | 8.28× |
| `rezolus_cpu_aperf_chain_total` | 1 | 29 | 0.42 | 3.42 | 8.11× |
| `counter_rate_sum_scaled` | 1 | 29 | 0.08 | 0.62 | 7.84× |
| `gauge_max_bare` | 1 | 29 | 0.06 | 0.41 | 7.26× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 29 | 0.30 | 1.84 | 6.24× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 348 | 0.16 | 0.90 | 5.80× |
| `counter_ratio_generic` | 2 | 58 | 0.26 | 1.41 | 5.42× |
| `counter_ratio_scaled` | 1 | 29 | 0.20 | 1.04 | 5.17× |
| `gauge_avg_scaled` | 6 | 174 | 0.09 | 0.43 | 4.91× |
| `counter_total_sum_generic` | 9 | 261 | 0.11 | 0.53 | 4.77× |
| `counter_irate_total_scaled` | 1 | 29 | 0.07 | 0.32 | 4.43× |
| `counter_irate_with_labels_scaled` | 3 | 87 | 0.20 | 0.87 | 4.41× |
| `softirq_irate_total_by_kind` | 10 | 290 | 0.11 | 0.46 | 4.17× |
| `counter_irate_sum_with_labels` | 84 | 2436 | 0.07 | 0.28 | 3.88× |
| `counter_irate_total_mul` | 2 | 58 | 0.09 | 0.27 | 3.00× |
| `counter_irate_ratio_with_labels` | 3 | 87 | 0.13 | 0.29 | 2.30× |
| `memory_util_pct` | 1 | 29 | 0.07 | 0.15 | 2.12× |
| `gauge_subtract` | 1 | 29 | 0.05 | 0.10 | 1.98× |
| `gauge_ratio_with_labels_ignoring` | 2 | 58 | 0.09 | 0.17 | 1.98× |
| `gauge_sum_by_g_bare` | 1 | 29 | 0.05 | 0.10 | 1.91× |
| `gauge_bare` | 5 | 145 | 0.06 | 0.11 | 1.90× |
| `gauge_sum_by_g_with_labels` | 6 | 174 | 0.06 | 0.10 | 1.79× |
| `gauge_sum_by_g_scaled` | 7 | 203 | 0.06 | 0.10 | 1.76× |
| `counter_ratio_by_id_complement` | 1 | 29 | 0.05 | 0.08 | 1.50× |
| `gauge_sum_scaled` | 1 | 29 | 0.06 | 0.09 | 1.48× |
| `gauge_sum_with_labels` | 5 | 145 | 0.06 | 0.09 | 1.44× |
| `softirq_time_pct_by_id_by_kind` | 5 | 145 | 0.06 | 0.08 | 1.41× |
| `softirq_irate_by_id_by_kind` | 5 | 145 | 0.06 | 0.08 | 1.38× |
| `gauge_bare_with_labels` | 32 | 928 | 0.05 | 0.07 | 1.38× |
| `counter_ratio_complement` | 1 | 29 | 0.05 | 0.07 | 1.34× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 29 | 0.05 | 0.07 | 1.29× |
| `rezolus_bpf_run_time_sec` | 1 | 29 | 0.06 | 0.07 | 1.25× |
| `counter_irate_subtract_with_labels` | 2 | 58 | 0.06 | 0.07 | 1.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 29 | 0.06 | 0.07 | 1.22× |
| `counter_irate_by_g_with_labels` | 20 | 580 | 0.07 | 0.08 | 1.10× |
| `gauge_sum_bare` | 2 | 58 | 0.08 | 0.09 | 1.07× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 87 | 0.07 | 0.07 | 1.07× |
| `counter_ratio_by_g_with_labels` | 1 | 29 | 0.13 | 0.10 | 0.72× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 7.78× | 0.08 | 0.64 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.74× | 0.09 | 0.62 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.33× | 0.34 | 1.81 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 5.10× | 0.06 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="event"}[5m]))` |
| 5.06× | 0.06 | 0.29 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 4.93× | 0.06 | 0.29 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 4.90× | 0.06 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="lock"}[5m]))` |
| 4.90× | 0.06 | 0.28 | `counter_irate_sum_with_labels` | `sum(irate(blockio_bytes{op="read"}[5m]))` |
| 4.86× | 0.06 | 0.29 | `counter_total_sum_generic` | `sum(irate(tcp_retransmit[5m]))` |
| 4.84× | 0.05 | 0.26 | `counter_irate_sum_with_labels` | `sum(irate(cache_misses{source="cachecannon"}[5s]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.65× | 0.08 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.67× | 0.10 | 0.06 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.77× | 0.07 | 0.05 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.91× | 0.10 | 0.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 1.10× | 0.08 | 0.08 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="block"}[5m])) / cpu_cores / 1000000000` |
| 1.10× | 0.07 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="event",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.12× | 0.07 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="query",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.15× | 0.07 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="poll",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.18× | 0.06 | 0.07 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="user",name=~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 1.20× | 0.07 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="sleep",name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 7.35× | 0.08 | 0.60 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.27× | 0.10 | 0.61 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.22× | 0.32 | 1.67 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 5.06× | 0.56 | 2.84 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 4.96× | 0.05 | 0.26 | `counter_irate_sum_with_labels` | `sum(irate(blockio_bytes{op="read"}[5m]))` |
| 4.80× | 0.07 | 0.36 | `counter_total_sum_generic` | `sum(irate(blockio_operations[5m]))` |
| 4.78× | 0.10 | 0.48 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 4.78× | 0.09 | 0.44 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="timer"}[5m]))` |
| 4.74× | 0.15 | 0.73 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 4.72× | 0.08 | 0.37 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.60× | 0.09 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.86× | 0.10 | 0.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.90× | 0.07 | 0.06 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.94× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="write",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.94× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="poll",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.96× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="read",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.00× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="sleep",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.00× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="event",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.00× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="memory",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.01× | 0.11 | 0.11 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.12× | 0.08 | 0.64 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.71× | 0.09 | 0.63 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.46× | 0.05 | 0.28 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 5.43× | 0.05 | 0.27 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 5.18× | 0.33 | 1.72 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 5.11× | 0.05 | 0.27 | `counter_irate_sum_with_labels` | `sum(irate(blockio_bytes{op="read"}[5m]))` |
| 5.08× | 0.05 | 0.27 | `counter_irate_sum_with_labels` | `sum(irate(blockio_operations{op="read"}[5m]))` |
| 4.98× | 0.06 | 0.28 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 4.96× | 0.06 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="socket"}[5m]))` |
| 4.80× | 0.10 | 0.46 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="tasklet"}[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.62× | 0.09 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.83× | 0.07 | 0.05 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.98× | 0.10 | 0.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 1.05× | 0.07 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="socket",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.13× | 0.08 | 0.09 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="block"}[5m])) / cpu_cores / 1000000000` |
| 1.14× | 0.10 | 0.11 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.14× | 0.07 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="ipc",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.16× | 0.06 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="write",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.17× | 0.07 | 0.09 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="other",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.18× | 0.07 | 0.08 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="user",name=~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 9.47× | 0.10 | 0.92 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 8.01× | 0.09 | 0.74 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 7.79× | 0.10 | 0.77 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 7.11× | 0.12 | 0.83 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 7.10× | 0.09 | 0.67 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 6.93× | 0.39 | 2.72 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 6.41× | 0.07 | 0.48 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 5.85× | 0.13 | 0.77 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="sched"}[5m]))` |
| 5.70× | 0.13 | 0.73 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 5.52× | 0.11 | 0.62 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.67× | 0.10 | 0.07 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.69× | 0.16 | 0.11 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.75× | 0.11 | 0.09 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.87× | 0.08 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="process",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.88× | 0.08 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="yield",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.90× | 0.12 | 0.10 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="block"}[5m])) / cpu_cores / 1000000000` |
| 0.91× | 0.09 | 0.09 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="lock",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.94× | 0.08 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.98× | 0.08 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="query",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.02× | 0.08 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 7.95× | 0.12 | 0.92 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.88× | 0.28 | 1.94 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 6.77× | 0.10 | 0.71 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 6.54× | 0.29 | 1.90 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 6.49× | 0.28 | 1.83 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 6.33× | 0.29 | 1.82 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 6.32× | 0.58 | 3.65 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 6.21× | 0.13 | 0.82 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 6.13× | 0.59 | 3.61 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 6.09× | 0.25 | 1.50 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.43× | 0.09 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 0.45× | 0.09 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 0.46× | 0.08 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 0.48× | 0.08 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 0.48× | 0.08 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 0.52× | 0.11 | 0.06 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 0.54× | 0.13 | 0.07 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.54× | 0.18 | 0.10 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.57× | 0.09 | 0.05 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 0.61× | 0.07 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 7.13× | 0.07 | 0.53 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 6.68× | 0.09 | 0.61 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 5.57× | 0.39 | 2.19 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 5.50× | 0.10 | 0.54 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.14× | 0.42 | 2.17 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 5.09× | 0.17 | 0.84 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 5.03× | 0.18 | 0.88 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 4.95× | 0.17 | 0.86 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 4.95× | 0.11 | 0.52 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 4.90× | 0.10 | 0.51 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.64× | 0.07 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.79× | 0.06 | 0.04 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 0.83× | 0.07 | 0.06 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.85× | 0.09 | 0.08 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.96× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_migrations{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.97× | 0.05 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang"}[5s]))` |
| 0.99× | 0.06 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="filesystem",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.02× | 0.06 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="lock",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.03× | 0.06 | 0.06 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.05× | 0.04 | 0.05 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang"}` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.99× | 0.09 | 0.79 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 7.33× | 0.06 | 0.43 | `gauge_max_bare` | `max(gpu_temperature)` |
| 7.03× | 0.07 | 0.48 | `gauge_avg_scaled` | `avg(gpu_memory_utilization) / 100` |
| 6.38× | 0.07 | 0.42 | `gauge_avg_scaled` | `avg(gpu_utilization) / 100` |
| 5.63× | 0.15 | 0.85 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 5.25× | 0.09 | 0.50 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="block"}[5m]))` |
| 5.23× | 0.11 | 0.58 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 5.07× | 0.10 | 0.50 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="block"}[5m])) / 1000000000` |
| 5.07× | 0.55 | 2.79 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 5.02× | 0.16 | 0.81 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.29× | 0.12 | 0.04 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 0.44× | 0.09 | 0.04 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 0.45× | 0.07 | 0.03 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.50× | 0.06 | 0.03 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.52× | 0.06 | 0.03 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.63× | 0.08 | 0.05 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.68× | 0.12 | 0.08 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.72× | 0.07 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.77× | 0.08 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="socket",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.77× | 0.07 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="query",name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 6.98× | 0.15 | 1.07 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 6.59× | 0.09 | 0.59 | `gauge_max_bare` | `max(gpu_temperature)` |
| 6.36× | 0.11 | 0.69 | `gauge_avg_scaled` | `avg(gpu_memory_utilization) / 100` |
| 5.86× | 0.10 | 0.58 | `gauge_avg_scaled` | `avg(gpu_utilization) / 100` |
| 5.77× | 0.21 | 1.22 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 5.67× | 0.19 | 1.10 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="hrtimer"}[5m]))` |
| 5.58× | 0.10 | 0.56 | `gauge_bare` | `memory_total` |
| 5.56× | 0.12 | 0.64 | `gauge_bare` | `memory_available` |
| 5.48× | 0.10 | 0.56 | `gauge_bare` | `memory_cached` |
| 5.34× | 0.11 | 0.57 | `gauge_bare` | `memory_free` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.42× | 0.12 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.42× | 0.11 | 0.05 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.44× | 0.09 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.45× | 0.11 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 0.45× | 0.10 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 0.46× | 0.13 | 0.06 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 0.49× | 0.10 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 0.52× | 0.10 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 0.56× | 0.09 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-prefill"}[5s]))` |
| 0.58× | 0.13 | 0.07 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 11.60× | 0.07 | 0.85 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 9.54× | 0.06 | 0.59 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 9.41× | 0.54 | 5.08 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 8.89× | 0.36 | 3.21 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 8.55× | 0.36 | 3.08 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 8.40× | 0.05 | 0.45 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 8.06× | 0.07 | 0.58 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 7.99× | 0.07 | 0.59 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 7.19× | 0.05 | 0.36 | `gauge_max_bare` | `max(gpu_temperature)` |
| 7.02× | 0.19 | 1.33 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.84× | 0.08 | 0.07 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.86× | 0.12 | 0.10 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.94× | 0.07 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="timer",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.95× | 0.09 | 0.08 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 0.99× | 0.08 | 0.08 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 1.03× | 0.10 | 0.11 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.08× | 0.08 | 0.09 | `gauge_sum_bare` | `sum(gpu_pcie_bandwidth)` |
| 1.09× | 0.08 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="sleep",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.10× | 0.06 | 0.06 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 1.12× | 0.05 | 0.06 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang-prefill"}[5s]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 11.09× | 0.09 | 0.95 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 7.80× | 0.11 | 0.90 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="block"}[5m])) / 1000000000` |
| 7.07× | 0.12 | 0.83 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="block"}[5m]))` |
| 7.07× | 0.10 | 0.72 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.72× | 0.07 | 0.45 | `gauge_max_bare` | `max(gpu_temperature)` |
| 6.70× | 0.08 | 0.53 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 6.66× | 0.50 | 3.35 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 6.53× | 0.88 | 5.76 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 6.12× | 0.11 | 0.70 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 6.04× | 0.09 | 0.54 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.69× | 0.15 | 0.10 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.73× | 0.10 | 0.07 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 0.74× | 0.10 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="lock",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.76× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="timer",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.76× | 0.11 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.76× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="time",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.77× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="sleep",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.78× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="process",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.78× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="poll",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.78× | 0.10 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="filesystem",name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 11.78× | 0.08 | 0.88 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 10.14× | 0.57 | 5.80 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 9.87× | 0.06 | 0.64 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 8.15× | 0.08 | 0.63 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 8.11× | 0.42 | 3.42 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 7.97× | 0.06 | 0.44 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 7.84× | 0.08 | 0.62 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 7.67× | 0.40 | 3.08 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 7.26× | 0.06 | 0.41 | `gauge_max_bare` | `max(gpu_temperature)` |
| 6.58× | 0.22 | 1.42 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.72× | 0.13 | 0.10 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.75× | 0.13 | 0.10 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 0.76× | 0.10 | 0.07 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 0.80× | 0.10 | 0.08 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.86× | 0.07 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.93× | 0.08 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_migrations{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.94× | 0.09 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.98× | 0.08 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="read",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.99× | 0.08 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="poll",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.02× | 0.07 | 0.07 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="system",name=~"__SELECTED_CGROUPS__"}[5m])) / 100000000…` |

