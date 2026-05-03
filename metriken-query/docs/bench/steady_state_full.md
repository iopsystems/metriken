# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 2
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 326.7 ms | 21.1 ms | 7.7 ms | 264.7 ms | 1.4 ms | 254.3 ms |
| `AB_base_pin.parquet` | 3.1 | 317.8 ms | 8.4 ms | 3.1 ms | 229.3 ms | 0.1 ms | 241.3 ms |
| `AB_level.parquet` | 3.3 | 340.8 ms | 9.6 ms | 2.9 ms | 244.4 ms | 0.1 ms | 242.2 ms |
| `AB_level_pin.parquet` | 2.8 | 327.1 ms | 11.2 ms | 2.8 ms | 228.6 ms | 0.2 ms | 238.2 ms |
| `cachecannon.parquet` | 3.8 | 346.9 ms | 7.8 ms | 2.5 ms | 264.3 ms | 0.2 ms | 277.7 ms |
| `demo.parquet` | 1.2 | 108.6 ms | 7.0 ms | 2.8 ms | 97.1 ms | 0.1 ms | 102.7 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 689.4 ms | 10.1 ms | 2.8 ms | 1310.2 ms | 0.2 ms | 1116.1 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2433.7 ms | 23.7 ms | 3.2 ms | 3915.0 ms | 0.6 ms | 4748.2 ms |
| `sglang_gemma3.parquet` | 2.1 | 277.6 ms | 23.9 ms | 3.8 ms | 445.1 ms | 0.4 ms | 392.3 ms |
| `vllm.parquet` | 4.0 | 327.7 ms | 13.8 ms | 2.3 ms | 611.5 ms | 0.5 ms | 410.9 ms |
| `vllm_gemma3.parquet` | 2.1 | 265.0 ms | 10.4 ms | 2.8 ms | 488.3 ms | 0.6 ms | 485.1 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 258 | 0 | 5 | 50 | 0 | 0.079 | 0.237 | 3.01× |
| `AB_base_pin.parquet` | 258 | 0 | 5 | 50 | 0 | 0.077 | 0.243 | 3.15× |
| `AB_level.parquet` | 258 | 0 | 5 | 50 | 0 | 0.079 | 0.240 | 3.05× |
| `AB_level_pin.parquet` | 258 | 0 | 5 | 50 | 0 | 0.076 | 0.230 | 3.05× |
| `cachecannon.parquet` | 254 | 0 | 5 | 54 | 0 | 0.070 | 0.230 | 3.26× |
| `demo.parquet` | 263 | 0 | 5 | 45 | 0 | 0.057 | 0.153 | 2.66× |
| `disagg/disagg-sglang.parquet` | 241 | 0 | 5 | 67 | 0 | 0.078 | 0.249 | 3.19× |
| `disagg/sglang-nixl-16c.parquet` | 227 | 0 | 5 | 81 | 0 | 0.191 | 0.434 | 2.28× |
| `sglang_gemma3.parquet` | 247 | 0 | 5 | 61 | 0 | 0.083 | 0.331 | 4.00× |
| `vllm.parquet` | 245 | 0 | 5 | 63 | 0 | 0.096 | 0.347 | 3.60× |
| `vllm_gemma3.parquet` | 245 | 0 | 5 | 63 | 0 | 0.080 | 0.300 | 3.75× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.74 | 8.99× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.60 | 6.56× |
| `counter_ratio_scaled` | 1 | 4 | 0.20 | 1.21 | 6.14× |
| `counter_ratio_generic` | 2 | 8 | 0.19 | 1.10 | 5.85× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.30 | 4.76× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.41 | 4.21× |
| `counter_irate_sum_with_labels` | 84 | 336 | 0.06 | 0.23 | 4.14× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 0.41 | 3.50× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 0.34 | 3.48× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.48 | 1.60 | 3.35× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.23 | 0.77 | 3.27× |
| `gauge_bare` | 5 | 20 | 0.04 | 0.13 | 3.26× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.25 | 2.87× |
| `gauge_subtract` | 1 | 4 | 0.04 | 0.12 | 2.84× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.09 | 2.33× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.09 | 1.94× |
| `softirq_irate_by_id_by_kind` | 5 | 20 | 0.05 | 0.09 | 1.91× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.14 | 1.90× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.11 | 1.83× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.08 | 1.79× |
| `gauge_bare_with_labels` | 33 | 132 | 0.05 | 0.08 | 1.76× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.08 | 1.76× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.08 | 1.74× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.05 | 0.08 | 1.71× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.08 | 1.71× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.08 | 1.70× |
| `counter_irate_by_id_with_labels` | 4 | 16 | 0.05 | 0.08 | 1.70× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.09 | 1.60× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.08 | 1.59× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.07 | 1.50× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.08 | 1.48× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.07 | 1.46× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.08 | 1.45× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.06 | 1.42× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.07 | 1.41× |
| `softirq_time_pct_by_id_by_kind` | 5 | 20 | 0.05 | 0.07 | 1.38× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.05 | 0.07 | 1.36× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.06 | 0.08 | 1.35× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.10 | 1.33× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.06 | 0.08 | 1.30× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.08 | 0.09 | 1.07× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.08 | 1.05× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.04 | 0.85× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.06 | 0.75× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.04 | 0.73× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.73 | 8.82× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.20 | 7.46× |
| `counter_ratio_scaled` | 1 | 4 | 0.16 | 1.17 | 7.36× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.59 | 6.61× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.08 | 0.39 | 5.09× |
| `counter_irate_sum_with_labels` | 84 | 336 | 0.06 | 0.26 | 4.58× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.29 | 4.56× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.33 | 4.15× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 1.59 | 4.09× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 0.72 | 3.95× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.11 | 0.41 | 3.78× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.24 | 3.61× |
| `gauge_bare` | 5 | 20 | 0.04 | 0.12 | 2.95× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.12 | 2.62× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.15 | 2.12× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.09 | 1.90× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.08 | 1.88× |
| `gauge_sum_scaled` | 1 | 4 | 0.05 | 0.09 | 1.82× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.11 | 1.79× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.08 | 1.79× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.08 | 1.77× |
| `gauge_bare_with_labels` | 33 | 132 | 0.04 | 0.08 | 1.77× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.08 | 1.76× |
| `softirq_irate_by_id_by_kind` | 5 | 20 | 0.05 | 0.09 | 1.69× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.09 | 1.68× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.08 | 1.66× |
| `counter_irate_by_id_with_labels` | 4 | 16 | 0.05 | 0.08 | 1.65× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.05 | 0.08 | 1.61× |
| `softirq_time_pct_by_id_by_kind` | 5 | 20 | 0.05 | 0.09 | 1.60× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.08 | 1.59× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.09 | 1.55× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.08 | 1.53× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.08 | 1.53× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.05 | 0.08 | 1.47× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.08 | 1.45× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.08 | 1.39× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.09 | 1.39× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.04 | 0.06 | 1.26× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.07 | 0.09 | 1.25× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.10 | 1.22× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.09 | 0.10 | 1.09× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.05 | 1.02× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.06 | 0.06 | 0.96× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.05 | 0.78× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.05 | 0.68× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_scaled` | 1 | 4 | 0.16 | 1.22 | 7.68× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.61 | 7.05× |
| `counter_ratio_generic` | 2 | 8 | 0.18 | 1.18 | 6.49× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 0.68 | 6.04× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 1.77 | 4.61× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.40 | 4.33× |
| `counter_irate_sum_with_labels` | 84 | 336 | 0.06 | 0.25 | 4.32× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.14 | 0.59 | 4.30× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.40 | 4.26× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.30 | 4.03× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.28 | 3.77× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 0.43 | 3.47× |
| `gauge_bare` | 5 | 20 | 0.04 | 0.13 | 3.12× |
| `gauge_subtract` | 1 | 4 | 0.04 | 0.12 | 2.57× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.15 | 2.20× |
| `gauge_bare_with_labels` | 33 | 132 | 0.04 | 0.07 | 1.93× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.08 | 1.87× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.10 | 1.81× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.08 | 1.78× |
| `counter_irate_by_id_with_labels` | 4 | 16 | 0.04 | 0.07 | 1.77× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.05 | 0.08 | 1.74× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.08 | 1.71× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.05 | 0.08 | 1.70× |
| `softirq_time_pct_by_id_by_kind` | 5 | 20 | 0.05 | 0.08 | 1.65× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.07 | 1.61× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.07 | 1.56× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.08 | 1.56× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.06 | 1.56× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.05 | 1.51× |
| `softirq_irate_by_id_by_kind` | 5 | 20 | 0.06 | 0.08 | 1.51× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.10 | 1.49× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.05 | 1.47× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.07 | 1.47× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.04 | 1.36× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.04 | 1.27× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.04 | 0.05 | 1.25× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.06 | 0.07 | 1.14× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.08 | 1.14× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.07 | 1.06× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.05 | 0.05 | 0.95× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.07 | 0.91× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.09 | 0.08 | 0.88× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.06 | 0.84× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.05 | 0.66× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.05 | 0.64× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.68 | 7.57× |
| `counter_ratio_scaled` | 1 | 4 | 0.16 | 1.22 | 7.51× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.58 | 6.42× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 1.00 | 4.92× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 1.74 | 4.62× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.33 | 4.39× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.36 | 4.04× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.25 | 3.95× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.16 | 0.62 | 3.89× |
| `counter_irate_sum_with_labels` | 84 | 336 | 0.06 | 0.22 | 3.74× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.11 | 0.40 | 3.64× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.27 | 3.52× |
| `gauge_bare` | 5 | 20 | 0.04 | 0.12 | 3.17× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.12 | 2.37× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.13 | 1.98× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.07 | 1.93× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.09 | 1.86× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.04 | 0.07 | 1.68× |
| `softirq_irate_by_id_by_kind` | 5 | 20 | 0.05 | 0.09 | 1.64× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.05 | 0.07 | 1.56× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.07 | 1.55× |
| `counter_irate_by_id_with_labels` | 4 | 16 | 0.05 | 0.07 | 1.52× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.06 | 1.47× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.06 | 1.44× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.07 | 1.42× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.06 | 1.41× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.06 | 1.40× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.07 | 1.38× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.07 | 1.35× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.07 | 1.33× |
| `softirq_time_pct_by_id_by_kind` | 5 | 20 | 0.06 | 0.08 | 1.33× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.04 | 1.32× |
| `gauge_bare_with_labels` | 33 | 132 | 0.03 | 0.04 | 1.31× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.05 | 1.30× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.04 | 1.29× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.04 | 1.21× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.04 | 1.20× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.05 | 1.18× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.07 | 1.15× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.06 | 0.07 | 1.15× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.07 | 0.07 | 1.13× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.04 | 0.97× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.09 | 0.83× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.06 | 0.67× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.05 | 0.53× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.96 | 9.44× |
| `counter_ratio_scaled` | 1 | 4 | 0.51 | 3.19 | 6.29× |
| `counter_ratio_generic` | 2 | 8 | 0.49 | 3.03 | 6.17× |
| `counter_total_sum_generic` | 10 | 40 | 0.10 | 0.58 | 5.96× |
| `gauge_bare` | 5 | 20 | 0.05 | 0.29 | 5.74× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.34 | 5.59× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.31 | 4.72× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.37 | 4.60× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.26 | 4.37× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.23 | 0.96 | 4.14× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.16 | 2.84× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.18 | 2.30× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.08 | 1.90× |
| `gauge_bare_with_labels` | 33 | 132 | 0.04 | 0.08 | 1.81× |
| `counter_irate_by_id_with_labels` | 4 | 16 | 0.05 | 0.09 | 1.77× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.08 | 1.75× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.08 | 1.74× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.08 | 1.73× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.08 | 1.68× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.07 | 1.65× |
| `softirq_irate_by_id_by_kind` | 4 | 16 | 0.06 | 0.10 | 1.55× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.08 | 1.52× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.07 | 1.50× |
| `softirq_time_pct_by_id_by_kind` | 4 | 16 | 0.06 | 0.09 | 1.48× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.07 | 1.45× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.06 | 1.44× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.09 | 1.41× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.06 | 0.09 | 1.37× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.06 | 0.08 | 1.36× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.06 | 0.08 | 1.32× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.09 | 1.30× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.08 | 1.29× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.06 | 1.23× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.06 | 1.23× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.06 | 1.22× |
| `gauge_max_bare` | 1 | 4 | 0.05 | 0.05 | 1.14× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.06 | 1.12× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.07 | 0.08 | 1.06× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.04 | 0.99× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.05 | 0.05 | 0.90× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.05 | 0.89× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.09 | 0.89× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.13 | 0.11 | 0.87× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.16 | 0.08 | 0.48× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.05 | 6.55× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 0.50 | 6.12× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 2.22 | 5.90× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.28 | 4.53× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.39 | 4.41× |
| `counter_total_sum_generic` | 11 | 44 | 0.06 | 0.24 | 4.31× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.10 | 0.42 | 4.14× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.25 | 3.82× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.07 | 0.24 | 3.25× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.10 | 2.59× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.07 | 2.29× |
| `gauge_bare` | 5 | 20 | 0.05 | 0.09 | 1.87× |
| `counter_irate_by_id_generic` | 4 | 16 | 0.04 | 0.07 | 1.82× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.05 | 0.09 | 1.77× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.05 | 0.09 | 1.75× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.09 | 1.73× |
| `softirq_irate_by_id_by_kind` | 4 | 16 | 0.05 | 0.07 | 1.40× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.05 | 1.37× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.09 | 1.35× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.04 | 1.32× |
| `gauge_bare_with_labels` | 36 | 144 | 0.03 | 0.04 | 1.32× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.05 | 1.30× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.04 | 0.05 | 1.30× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.04 | 1.28× |
| `counter_irate_by_id_with_labels` | 4 | 16 | 0.03 | 0.04 | 1.28× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.04 | 1.27× |
| `softirq_time_pct_by_id_by_kind` | 4 | 16 | 0.04 | 0.05 | 1.26× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.05 | 1.24× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.07 | 1.19× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.06 | 0.07 | 1.19× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.05 | 1.17× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.06 | 1.16× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.06 | 0.07 | 1.13× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.04 | 1.12× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.04 | 1.12× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.04 | 1.06× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.04 | 1.03× |
| `gauge_max_bare` | 1 | 4 | 0.05 | 0.05 | 0.99× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.08 | 0.08 | 0.94× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.04 | 0.91× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.05 | 0.87× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.04 | 0.80× |
| `counter_ratio_scaled` | 1 | 4 | 0.05 | 0.04 | 0.77× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.06 | 0.04 | 0.74× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.07 | 0.04 | 0.61× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.08 | 0.04 | 0.52× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 0.77 | 8.86× |
| `gauge_max_bare` | 1 | 4 | 0.05 | 0.42 | 8.19× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.09 | 0.62 | 6.75× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 0.79 | 5.86× |
| `counter_ratio_scaled` | 1 | 4 | 0.29 | 1.59 | 5.48× |
| `gauge_bare` | 5 | 20 | 0.05 | 0.27 | 5.21× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.15 | 0.73 | 4.80× |
| `counter_total_sum_generic` | 9 | 36 | 0.10 | 0.43 | 4.50× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.31 | 1.34 | 4.28× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.30 | 3.97× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.16 | 0.64 | 3.95× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.35 | 3.86× |
| `counter_irate_sum_with_labels` | 83 | 332 | 0.07 | 0.25 | 3.66× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.26 | 3.10× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.18 | 2.85× |
| `memory_util_pct` | 1 | 4 | 0.09 | 0.21 | 2.51× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.07 | 1.79× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.09 | 1.73× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.05 | 0.07 | 1.50× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.08 | 1.49× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.05 | 0.07 | 1.48× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.07 | 1.47× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.09 | 1.44× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.05 | 0.07 | 1.43× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.07 | 1.38× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.07 | 1.37× |
| `softirq_time_pct_by_id_by_kind` | 3 | 12 | 0.06 | 0.08 | 1.30× |
| `gauge_bare_with_labels` | 27 | 108 | 0.04 | 0.05 | 1.30× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.09 | 1.29× |
| `softirq_irate_by_id_by_kind` | 3 | 12 | 0.06 | 0.07 | 1.28× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.07 | 1.25× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.06 | 0.08 | 1.21× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.07 | 1.20× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.06 | 0.07 | 1.14× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.08 | 1.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.07 | 1.13× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.07 | 1.13× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.07 | 0.07 | 1.08× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.07 | 0.91× |
| `counter_ratio_generic` | 2 | 8 | 0.08 | 0.07 | 0.88× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.09 | 0.08 | 0.87× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.06 | 0.04 | 0.65× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.07 | 0.04 | 0.64× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 4 | 0.07 | 0.03 | 0.41× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.13 | 1.13 | 8.55× |
| `gauge_max_bare` | 1 | 4 | 0.08 | 0.56 | 7.36× |
| `gauge_bare` | 5 | 20 | 0.10 | 0.65 | 6.41× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.16 | 0.97 | 6.15× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.25 | 1.14 | 4.62× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.13 | 0.53 | 4.25× |
| `gauge_subtract` | 1 | 4 | 0.12 | 0.45 | 3.84× |
| `counter_total_sum_generic` | 9 | 36 | 0.28 | 1.05 | 3.78× |
| `counter_rate_bare_generic` | 2 | 8 | 0.58 | 2.10 | 3.65× |
| `counter_irate_total_mul` | 2 | 8 | 0.15 | 0.47 | 3.09× |
| `counter_irate_sum_with_labels` | 73 | 292 | 0.12 | 0.36 | 2.91× |
| `memory_util_pct` | 1 | 4 | 0.18 | 0.53 | 2.86× |
| `counter_irate_total_scaled` | 1 | 4 | 0.19 | 0.55 | 2.83× |
| `counter_ratio_generic` | 2 | 8 | 0.88 | 2.32 | 2.64× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.68 | 1.71 | 2.50× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.44 | 1.08 | 2.42× |
| `counter_ratio_scaled` | 1 | 4 | 0.97 | 2.33 | 2.39× |
| `counter_ratio_by_id_generic` | 1 | 4 | 0.05 | 0.09 | 1.88× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.04 | 0.08 | 1.77× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.10 | 1.71× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.09 | 0.13 | 1.55× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.08 | 1.50× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.10 | 0.14 | 1.42× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.10 | 1.35× |
| `counter_irate_by_id_generic` | 2 | 8 | 0.06 | 0.08 | 1.33× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.08 | 1.32× |
| `gauge_bare_with_labels` | 28 | 112 | 0.06 | 0.08 | 1.30× |
| `softirq_time_pct_by_id_by_kind` | 4 | 16 | 0.12 | 0.15 | 1.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.07 | 1.27× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.09 | 1.15× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.11 | 1.12× |
| `gauge_sum_bare` | 2 | 8 | 0.14 | 0.15 | 1.12× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.10 | 0.11 | 1.07× |
| `gauge_sum_with_labels` | 5 | 20 | 0.09 | 0.09 | 0.98× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.10 | 0.08 | 0.85× |
| `softirq_irate_by_id_by_kind` | 4 | 16 | 0.15 | 0.11 | 0.75× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.18 | 0.12 | 0.64× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.10 | 0.06 | 0.57× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.23 | 0.13 | 0.56× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.29 | 0.11 | 0.39× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.07 | 0.89 | 13.26× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.75 | 10.95× |
| `counter_ratio_scaled` | 1 | 4 | 0.16 | 1.64 | 10.06× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.33 | 3.25 | 9.75× |
| `gauge_max_bare` | 1 | 4 | 0.05 | 0.43 | 9.08× |
| `counter_ratio_generic` | 2 | 8 | 0.23 | 2.07 | 8.89× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 0.71 | 8.71× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.66 | 5.58 | 8.45× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 4 | 0.26 | 1.84 | 6.99× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.08 | 0.52 | 6.84× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.46 | 6.83× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.17 | 1.10 | 6.34× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 0.90 | 5.91× |
| `counter_total_sum_generic` | 9 | 36 | 0.09 | 0.50 | 5.67× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.36 | 5.47× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.30 | 4.34× |
| `counter_irate_sum_with_labels` | 84 | 336 | 0.07 | 0.29 | 4.33× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.14 | 2.53× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.11 | 2.27× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.09 | 2.24× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.12 | 2.18× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.17 | 2.17× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.11 | 2.14× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.12 | 1.96× |
| `gauge_bare_with_labels` | 32 | 128 | 0.04 | 0.07 | 1.71× |
| `gauge_bare` | 5 | 20 | 0.06 | 0.11 | 1.68× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.10 | 1.66× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.08 | 1.65× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.10 | 1.63× |
| `softirq_time_pct_by_id_by_kind` | 6 | 24 | 0.06 | 0.10 | 1.62× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.08 | 1.62× |
| `softirq_irate_by_id_by_kind` | 6 | 24 | 0.06 | 0.09 | 1.38× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.07 | 1.34× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.07 | 1.34× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.07 | 0.08 | 1.23× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.08 | 0.10 | 1.14× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.09 | 1.12× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.10 | 1.12× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.07 | 0.06 | 0.87× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.08 | 0.62× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.07 | 0.86 | 12.37× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.86 | 8.37× |
| `gauge_max_bare` | 1 | 4 | 0.05 | 0.44 | 8.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.44 | 3.38 | 7.69× |
| `counter_ratio_generic` | 2 | 8 | 0.31 | 1.88 | 6.10× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.78 | 4.70 | 6.06× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.66 | 5.74× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.11 | 0.62 | 5.41× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.42 | 5.20× |
| `counter_total_sum_generic` | 9 | 36 | 0.10 | 0.47 | 4.83× |
| `counter_irate_with_labels_scaled` | 2 | 8 | 0.34 | 1.57 | 4.64× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.35 | 4.60× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.33 | 4.59× |
| `counter_irate_sum_with_labels` | 83 | 332 | 0.06 | 0.27 | 4.36× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 4 | 0.32 | 1.35 | 4.27× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 0.68 | 3.91× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 0.43 | 3.50× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.07 | 2.12× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.12 | 2.07× |
| `gauge_bare` | 5 | 20 | 0.05 | 0.10 | 1.99× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.11 | 1.98× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.08 | 1.97× |
| `gauge_bare_with_labels` | 32 | 128 | 0.04 | 0.07 | 1.92× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.11 | 1.88× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.10 | 1.78× |
| `counter_irate_by_id_generic` | 1 | 4 | 0.05 | 0.09 | 1.77× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.15 | 1.74× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.04 | 0.08 | 1.73× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.07 | 1.73× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.08 | 1.70× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.10 | 1.68× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.10 | 1.56× |
| `softirq_irate_by_id_by_kind` | 5 | 20 | 0.06 | 0.09 | 1.50× |
| `softirq_time_pct_by_id_by_kind` | 5 | 20 | 0.07 | 0.10 | 1.50× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.08 | 1.49× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.07 | 1.31× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.10 | 1.27× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.08 | 0.09 | 1.18× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.09 | 1.16× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.09 | 0.09 | 1.04× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.09 | 0.83× |
| `counter_ratio_scaled` | 1 | 4 | 0.07 | 0.06 | 0.80× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.07 | 0.74 | 11.24× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.56 | 9.74× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.66 | 8.90× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.50 | 4.29 | 8.52× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.36 | 3.04 | 8.35× |
| `counter_ratio_scaled` | 1 | 4 | 0.16 | 1.30 | 8.16× |
| `counter_ratio_generic` | 2 | 8 | 0.21 | 1.70 | 8.01× |
| `counter_rate_bare_generic` | 2 | 8 | 0.07 | 0.57 | 7.94× |
| `gauge_max_bare` | 1 | 4 | 0.05 | 0.35 | 7.49× |
| `counter_irate_total_per_cpu_core_pct` | 1 | 4 | 0.28 | 1.74 | 6.34× |
| `counter_total_sum_generic` | 9 | 36 | 0.09 | 0.51 | 5.94× |
| `gauge_avg_scaled` | 6 | 24 | 0.06 | 0.35 | 5.72× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 0.81 | 5.69× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.14 | 0.77 | 5.56× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.30 | 4.03× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 0.40 | 3.43× |
| `counter_irate_sum_with_labels` | 84 | 336 | 0.06 | 0.21 | 3.41× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.23 | 3.07× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.15 | 2.45× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.06 | 0.15 | 2.41× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.05 | 0.11 | 2.36× |
| `gauge_bare` | 5 | 20 | 0.05 | 0.10 | 2.10× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.10 | 1.97× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.10 | 1.69× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.05 | 0.08 | 1.64× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.09 | 1.63× |
| `gauge_sum_scaled` | 1 | 4 | 0.05 | 0.08 | 1.47× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.08 | 1.41× |
| `softirq_irate_by_id_by_kind` | 5 | 20 | 0.07 | 0.10 | 1.39× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.08 | 1.34× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.09 | 1.34× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.08 | 1.33× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.07 | 1.30× |
| `softirq_time_pct_by_id_by_kind` | 5 | 20 | 0.08 | 0.09 | 1.21× |
| `counter_irate_by_g_with_labels_scaled` | 3 | 12 | 0.08 | 0.09 | 1.19× |
| `gauge_bare_with_labels` | 32 | 128 | 0.04 | 0.05 | 1.16× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.05 | 1.01× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.08 | 0.98× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.04 | 0.81× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.09 | 0.52× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.99× | 0.08 | 0.74 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 7.25× | 0.08 | 0.60 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.41× | 0.09 | 0.59 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 6.19× | 0.05 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="memory"}[5m]))` |
| 6.14× | 0.20 | 1.21 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 5.85× | 0.19 | 1.10 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 5.59× | 0.05 | 0.30 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 5.34× | 0.04 | 0.24 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 5.34× | 0.09 | 0.48 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="to"}[5m]))` |
| 5.27× | 0.05 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="time"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.73× | 0.05 | 0.04 | `rezolus_cpu_aperf_chain_per_id` | `sum by (id) (irate(cpu_tsc[5m])) * sum by (id) (irate(cpu_aperf[5m])) / sum by (id) (irate(cpu_mperf…` |
| 0.75× | 0.08 | 0.06 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.76× | 0.06 | 0.05 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.83× | 0.10 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.85× | 0.05 | 0.04 | `counter_ratio_complement` | `1 - sum(irate(cpu_l3_miss[5m])) / sum(irate(cpu_l3_access[5m]))` |
| 0.87× | 0.05 | 0.04 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="hi"}[5m]))` |
| 0.89× | 0.05 | 0.05 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-decode"}` |
| 1.00× | 0.05 | 0.05 | `gauge_bare_with_labels` | `smg_router_tpot_seconds{source="sglang-router"}` |
| 1.04× | 0.09 | 0.09 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="irq_poll"}[5m]))` |
| 1.05× | 0.08 | 0.08 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.82× | 0.08 | 0.73 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 7.46× | 0.16 | 1.20 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 7.36× | 0.16 | 1.17 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 6.66× | 0.09 | 0.59 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.40× | 0.09 | 0.57 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 6.17× | 0.05 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 6.03× | 0.05 | 0.28 | `counter_irate_sum_with_labels` | `sum(irate(blockio_operations{op="read"}[5m]))` |
| 5.79× | 0.05 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(network_packets{direction="transmit"}[5m]))` |
| 5.68× | 0.05 | 0.30 | `counter_irate_sum_with_labels` | `sum(irate(cache_misses{source="cachecannon"}[5s]))` |
| 5.50× | 0.55 | 3.00 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.68× | 0.08 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.75× | 0.05 | 0.04 | `counter_irate_subtract_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang"}[5s])) - sum(irate(sglang_num_aborted_requests_t…` |
| 0.77× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.78× | 0.07 | 0.05 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 0.81× | 0.06 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 0.88× | 0.07 | 0.06 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="system",name=~"__SELECTED_CGROUPS__"}[5m])) / 100000000…` |
| 0.89× | 0.05 | 0.04 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-prefill"}` |
| 0.96× | 0.06 | 0.06 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.96× | 0.05 | 0.04 | `gauge_sum_with_labels` | `sum(gpu_pcie_throughput{direction="receive"})` |
| 0.96× | 0.05 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.14× | 0.07 | 0.61 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 7.68× | 0.16 | 1.22 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 7.02× | 0.09 | 0.60 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 6.49× | 0.18 | 1.18 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 6.04× | 0.11 | 0.68 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 5.79× | 0.06 | 0.34 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="lock"}[5m]))` |
| 5.72× | 0.06 | 0.32 | `counter_irate_sum_with_labels` | `sum(irate(cache_hits{source="cachecannon"}[5s]))` |
| 5.62× | 0.05 | 0.29 | `counter_irate_sum_with_labels` | `sum(irate(blockio_operations{op="read"}[5m]))` |
| 5.59× | 0.14 | 0.80 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 5.23× | 0.51 | 2.68 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.64× | 0.08 | 0.05 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.66× | 0.08 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.73× | 0.06 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.83× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="yield",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.84× | 0.06 | 0.05 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="system",name=~"__SELECTED_CGROUPS__"}[5m])) / 100000000…` |
| 0.84× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang"}[5s]))` |
| 0.84× | 0.07 | 0.06 | `counter_ratio_by_id_complement` | `1 - sum by (id) (irate(cpu_l3_miss[5m])) / sum by (id) (irate(cpu_l3_access[5m]))` |
| 0.87× | 0.05 | 0.05 | `gauge_avg_scaled` | `avg(gpu_utilization) / 100` |
| 0.88× | 0.09 | 0.08 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.88× | 0.05 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-decode"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 7.57× | 0.09 | 0.68 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 7.51× | 0.16 | 1.22 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 7.38× | 0.08 | 0.59 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 7.30× | 0.08 | 0.58 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.56× | 0.08 | 0.51 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 6.33× | 0.09 | 0.57 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.72× | 0.06 | 0.34 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_tx"}[5m]))` |
| 5.47× | 0.34 | 1.84 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 5.39× | 0.09 | 0.49 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="sched"}[5m]))` |
| 5.15× | 0.04 | 0.22 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.53× | 0.09 | 0.05 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.67× | 0.08 | 0.06 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.73× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.78× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.82× | 0.05 | 0.04 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-decode"}` |
| 0.83× | 0.11 | 0.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.84× | 0.05 | 0.04 | `gauge_sum_with_labels` | `sum(gpu_memory{state="free"})` |
| 0.84× | 0.07 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="write",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.85× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.89× | 0.05 | 0.04 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 9.80× | 0.10 | 0.96 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 8.72× | 0.10 | 0.88 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 8.25× | 0.12 | 0.99 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 6.69× | 0.22 | 1.45 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 6.50× | 0.05 | 0.34 | `counter_irate_sum_with_labels` | `sum(irate(blockio_operations{op="read"}[5m]))` |
| 6.41× | 0.24 | 1.55 | `counter_total_sum_generic` | `sum(irate(scheduler_context_switch[5m]))` |
| 6.29× | 0.06 | 0.35 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 6.29× | 0.51 | 3.19 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 6.28× | 0.05 | 0.29 | `gauge_bare` | `memory_cached` |
| 6.25× | 0.25 | 1.58 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="to"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.40× | 0.08 | 0.03 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 0.48× | 0.16 | 0.08 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.48× | 0.07 | 0.03 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 0.48× | 0.10 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 0.61× | 0.09 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 0.61× | 0.07 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 0.61× | 0.08 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 0.84× | 0.04 | 0.04 | `gauge_bare_with_labels` | `smg_http_request_duration_seconds{source="sglang-router"}` |
| 0.86× | 0.06 | 0.05 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.87× | 0.13 | 0.11 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 6.55× | 0.16 | 1.05 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 6.20× | 0.08 | 0.50 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.01× | 0.36 | 2.17 | `counter_total_sum_generic` | `sum(irate(softirq[5m]))` |
| 5.90× | 0.38 | 2.22 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 5.78× | 0.08 | 0.47 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.46× | 0.12 | 0.68 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 5.41× | 0.15 | 0.80 | `counter_total_sum_generic` | `sum(irate(syscall[5m]))` |
| 5.23× | 0.05 | 0.27 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="poll"}[5m]))` |
| 5.14× | 0.05 | 0.25 | `counter_irate_sum_with_labels` | `sum(irate(blockio_bytes{op="read"}[5m]))` |
| 5.07× | 0.15 | 0.75 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.52× | 0.08 | 0.04 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 0.61× | 0.07 | 0.04 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.73× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.73× | 0.05 | 0.04 | `counter_ratio_generic` | `sum(irate(cpu_branch_misses[5m])) / sum(irate(cpu_branch_instructions[5m]))` |
| 0.74× | 0.06 | 0.04 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.75× | 0.05 | 0.04 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.76× | 0.06 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.76× | 0.10 | 0.08 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.77× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.77× | 0.05 | 0.04 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.86× | 0.09 | 0.77 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 8.80× | 0.08 | 0.68 | `counter_irate_ratio_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source…` |
| 8.19× | 0.05 | 0.42 | `gauge_max_bare` | `max(gpu_temperature)` |
| 7.18× | 0.07 | 0.47 | `gauge_avg_scaled` | `avg(gpu_memory_utilization) / 100` |
| 6.75× | 0.09 | 0.62 | `counter_irate_subtract_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang-decode"}[5s])) - sum(irate(sglang_num_aborted_req…` |
| 6.07× | 0.04 | 0.24 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-prefill"}` |
| 5.99× | 0.13 | 0.79 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 5.82× | 0.14 | 0.79 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 5.81× | 0.07 | 0.41 | `gauge_avg_scaled` | `avg(gpu_utilization) / 100` |
| 5.79× | 0.05 | 0.26 | `gauge_bare` | `memory_cached` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.38× | 0.07 | 0.03 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.41× | 0.07 | 0.03 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 0.49× | 0.06 | 0.03 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.50× | 0.09 | 0.05 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 0.51× | 0.06 | 0.03 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.61× | 0.08 | 0.05 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 0.64× | 0.07 | 0.04 | `rezolus_cpu_ipns_per_id` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate…` |
| 0.65× | 0.06 | 0.04 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 0.74× | 0.08 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="filesystem",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.76× | 0.07 | 0.05 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="write",name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8.55× | 0.13 | 1.13 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 7.40× | 0.09 | 0.65 | `gauge_bare` | `memory_cached` |
| 7.36× | 0.08 | 0.56 | `gauge_max_bare` | `max(gpu_temperature)` |
| 7.11× | 0.09 | 0.67 | `gauge_bare` | `memory_free` |
| 6.68× | 0.14 | 0.92 | `counter_irate_ratio_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source…` |
| 6.56× | 0.18 | 1.19 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="hrtimer"}[5m]))` |
| 6.41× | 0.10 | 0.65 | `gauge_bare` | `memory_buffers` |
| 6.15× | 0.16 | 0.97 | `counter_irate_subtract_with_labels` | `sum(irate(sglang_num_requests_total{source="sglang-decode"}[5s])) - sum(irate(sglang_num_aborted_req…` |
| 5.41× | 0.13 | 0.69 | `gauge_bare` | `memory_available` |
| 5.21× | 0.08 | 0.42 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-decode"}` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.39× | 0.11 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.39× | 0.29 | 0.11 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 0.40× | 0.08 | 0.03 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.41× | 0.12 | 0.05 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 0.41× | 0.09 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-prefill"}[5s]))` |
| 0.41× | 0.10 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 0.44× | 0.09 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 0.44× | 0.08 | 0.04 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-prefill"}` |
| 0.44× | 0.09 | 0.04 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.46× | 0.09 | 0.04 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 13.26× | 0.07 | 0.89 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 10.95× | 0.07 | 0.75 | `counter_irate_ratio_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source…` |
| 10.61× | 0.07 | 0.71 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 10.55× | 0.08 | 0.89 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 10.06× | 0.16 | 1.64 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 9.75× | 0.33 | 3.25 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 9.71× | 0.21 | 2.07 | `counter_ratio_generic` | `sum(irate(cpu_branch_misses[5m])) / sum(irate(cpu_branch_instructions[5m]))` |
| 9.10× | 0.12 | 1.10 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{state="system",name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 9.08× | 0.05 | 0.43 | `gauge_max_bare` | `max(gpu_temperature)` |
| 8.61× | 0.35 | 2.98 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.62× | 0.13 | 0.08 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.76× | 0.06 | 0.05 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="hi"}[5m]))` |
| 0.79× | 0.09 | 0.07 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.80× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang"}[5s]))` |
| 0.80× | 0.08 | 0.06 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.82× | 0.09 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="sleep",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.82× | 0.07 | 0.06 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 0.82× | 0.09 | 0.07 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="block"}[5m]))` |
| 0.83× | 0.05 | 0.04 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang"}` |
| 0.84× | 0.06 | 0.05 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="user",name=~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 12.37× | 0.07 | 0.86 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 9.84× | 0.04 | 0.44 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 8.37× | 0.10 | 0.86 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu…` |
| 8.26× | 0.05 | 0.44 | `gauge_max_bare` | `max(gpu_temperature)` |
| 7.86× | 0.09 | 0.70 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="block"}[5m])) / 1000000000` |
| 7.69× | 0.44 | 3.38 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 7.36× | 0.11 | 0.81 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="block"}[5m]))` |
| 6.94× | 0.10 | 0.66 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 6.73× | 0.28 | 1.88 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 6.06× | 0.78 | 4.70 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.76× | 0.11 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.78× | 0.09 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="lock",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.80× | 0.07 | 0.06 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 0.83× | 0.11 | 0.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.84× | 0.10 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="memory",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.84× | 0.06 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.86× | 0.12 | 0.10 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.95× | 0.10 | 0.09 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.97× | 0.07 | 0.06 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="query",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.99× | 0.09 | 0.09 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="system",name=~"__SELECTED_CGROUPS__"}[5m])) / 100000000…` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 11.24× | 0.07 | 0.74 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 9.86× | 0.07 | 0.66 | `counter_irate_ratio_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source…` |
| 9.74× | 0.06 | 0.56 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 9.34× | 0.06 | 0.56 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 9.32× | 0.18 | 1.70 | `counter_ratio_generic` | `sum(irate(cpu_branch_misses[5m])) / sum(irate(cpu_branch_instructions[5m]))` |
| 8.52× | 0.50 | 4.29 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 8.35× | 0.36 | 3.04 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 8.16× | 0.16 | 1.30 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 7.94× | 0.07 | 0.57 | `counter_rate_bare_generic` | `rate(memory_numa_local[5m])` |
| 7.74× | 0.34 | 2.62 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.52× | 0.17 | 0.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 0.54× | 0.10 | 0.05 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.69× | 0.08 | 0.05 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="ipc",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.80× | 0.06 | 0.05 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.81× | 0.05 | 0.04 | `counter_ratio_complement` | `1 - sum(irate(cpu_l3_miss[5m])) / sum(irate(cpu_l3_access[5m]))` |
| 0.83× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang-decode"}[5s]))` |
| 0.85× | 0.05 | 0.04 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang"}[5s]))` |
| 0.86× | 0.11 | 0.10 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.88× | 0.08 | 0.07 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="event",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.88× | 0.09 | 0.08 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="socket",name=~"__SELECTED_CGROUPS__"}[5m]))` |

