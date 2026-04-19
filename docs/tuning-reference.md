# Tuning Reference

`TuningConfig` is loaded from the `[tuning]` section of `ft.toml`. Every key is optional. If you omit a key, `ft` keeps the hard-coded default from `crates/frankenterm-core/src/tuning_config.rs`.

This reference is for operators tuning live fleets. The ranges below are starting points, not promises. Change one subsystem at a time, run `ft config validate`, then watch capture lag, queue depth, SQLite lock timing, memory pressure, search latency, and workflow completion time before moving further.

Before you tune a live deployment:

- For blessed 10, 50, and 200+ pane starting profiles with exact commands, evidence citations, and escalation rules, start with `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md`.
- For current rollout posture, hardware-tier defaults, and fallback guidance, use `docs/resize-user-facing-release-tuning-guidance-wa-1u90p.8.5.md`.
- For incident triage and rollback flow, use `docs/operator-playbook.md`.
- Treat this file as the knob-level reference, not the release-posture document.

## Sizing Profiles

- `10 panes`: small local swarm or single-operator setup where low latency matters more than peak throughput.
- `50 panes`: medium fleet. In most cases this is the baseline profile and the default settings are already appropriate.
- `200+ panes`: long-running, high-concurrency fleet where queue stability, memory headroom, and API fan-out matter more than shaving the last few milliseconds off every loop.

## `[tuning.runtime]`

These keys live under `[tuning.runtime]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `output_coalesce_window_ms` | `50` | ms | How long the runtime batches short output bursts before processing. Lower it for lower latency; raise it when bursty fleets create too many tiny writes. | `25-50` | `50-75` | `75-150` |
| `output_coalesce_max_delay_ms` | `200` | ms | Hard stop for coalescing. Raise only if you intentionally widen the coalesce window and still want bounded flush behavior. | `100-200` | `200-300` | `300-500` |
| `output_coalesce_max_bytes` | `262144` (`256 KiB`) | bytes | Maximum buffered bytes before a coalesced batch flushes. Raise it for very chatty panes; lower it if you want smaller SQLite writes. | `131072-262144` | `262144-524288` | `524288-1048576` |
| `telemetry_percentile_window` | `1024` | samples | How many recent lock and memory samples are retained for percentile telemetry. Raise when you want more stable percentiles over large fleets. | `512-1024` | `1024` | `1024-2048` |
| `resize_watchdog_warning_ms` | `2000` | ms | Warning threshold for stalled resize transactions. Lower for aggressive alerting; raise if slow remote panes create noise. | `1500-2500` | `2000-3000` | `3000-5000` |
| `resize_watchdog_critical_ms` | `8000` | ms | Critical threshold for stalled resize transactions. Keep it meaningfully above the warning threshold. | `5000-8000` | `8000-12000` | `12000-20000` |
| `resize_watchdog_stalled_limit` | `2` | consecutive stalls | How many critical stalls are tolerated before safe-mode style intervention is suggested. Raise only if you expect frequent transient resize stalls. | `1-2` | `2-3` | `2-4` |
| `resize_watchdog_sample_limit` | `8` | samples | Number of stalled transaction samples retained in watchdog payloads. Mostly a diagnostics knob. | `8` | `8` | `8-16` |
| `storage_lock_wait_warn_ms` | `15.0` | ms | Warning threshold for waiting on the SQLite lock. Lower it when lock contention is a primary SLO; raise it if bursty ingest makes the warning too noisy. | `10-20` | `15-30` | `25-50` |
| `storage_lock_hold_warn_ms` | `75.0` | ms | Warning threshold for holding the SQLite lock. Raise it cautiously if larger batched writes are expected. | `50-75` | `75-100` | `100-200` |
| `cursor_snapshot_memory_warn_bytes` | `67108864` (`64 MiB`) | bytes | Memory warning threshold for retained pane cursor snapshots. Raise when large fleets legitimately hold more in-memory cursor state. | `33554432-67108864` | `67108864-134217728` | `134217728-268435456` |
| `state_detection_max_age_secs` | `300` | s | How stale state-detection evidence may be before it is ignored. Lower it for sharper real-time state inference; raise it for slower fleets or intermittent capture. | `120-300` | `300` | `300-600` |

Validation:
- `output_coalesce_window_ms >= 5`
- `output_coalesce_max_delay_ms >= output_coalesce_window_ms`
- `output_coalesce_max_bytes >= 4096`
- `resize_watchdog_warning_ms < resize_watchdog_critical_ms`
- `resize_watchdog_stalled_limit >= 1`

## `[tuning.backpressure]`

These keys live under `[tuning.backpressure]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `warn_ratio` | `0.75` | fraction | Queue fullness ratio that marks the system unhealthy. Lower it when you want earlier warning and more headroom; raise it only if you can tolerate deeper queues. | `0.70-0.80` | `0.70-0.80` | `0.60-0.75` |

Validation:
- `warn_ratio` must stay in `[0.1, 0.99]`

## `[tuning.snapshot]`

These keys live under `[tuning.snapshot]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `trigger_bridge_tick_secs` | `30` | s | How often snapshot trigger maintenance runs. Lower for faster responsiveness; raise to reduce housekeeping overhead. | `15-30` | `30` | `30-60` |
| `idle_window_secs` | `300` | s | How long a pane must be idle before the runtime emits an idle-window trigger. Lower for aggressive snapshotting of quiet panes. | `120-300` | `300` | `300-900` |
| `memory_trigger_cooldown_secs` | `120` | s | Minimum spacing between memory-pressure-triggered snapshots. Raise it if memory pressure causes repeated snapshot churn. | `60-120` | `120` | `120-300` |

Validation:
- `trigger_bridge_tick_secs >= 5`

## `[tuning.ingest]`

These keys live under `[tuning.ingest]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `max_persist_segment_bytes` | `65536` (`64 KiB`) | bytes | Maximum size of a persisted output segment. Lower for finer-grained storage; raise for fewer, larger writes from chatty panes. | `32768-65536` | `65536` | `65536-131072` |
| `max_record_payload_bytes` | `67108864` (`64 MiB`) | bytes | Upper bound for a single tantivy ingestion record. Raise only if large captures are being truncated during indexing. | `16777216-67108864` | `67108864` | `67108864-134217728` |

Validation:
- No extra load-time validation beyond TOML typing.

## `[tuning.patterns]`

These keys live under `[tuning.patterns]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `max_seen_keys` | `1000` | entries | Dedup cache size for `(rule_id, pane_id, fingerprint)` tuples. Raise when many panes and many rules create eviction churn. | `500-1000` | `1000-4000` | `4000-16000` |
| `max_tail_size_bytes` | `2048` (`2 KiB`) | bytes | Per-pane tail retained for anchor matching. Raise when anchors need more context; lower if memory is tight and patterns are simple. | `1024-2048` | `2048-4096` | `4096-8192` |
| `bloom_false_positive_rate` | `0.01` | fraction | False-positive rate for the Bloom prefilter. Lower values spend more memory to avoid more regex work. | `0.01-0.02` | `0.005-0.01` | `0.001-0.01` |

Validation:
- `max_seen_keys >= 100`
- `max_tail_size_bytes >= 256`
- `bloom_false_positive_rate` must stay in `[0.001, 0.2]`

## `[tuning.policy]`

These keys live under `[tuning.policy]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `rate_limit_window_secs` | `60` | s | Sliding window used by the send-text rate limiter. Raise it for stricter smoothing; lower it for more burst tolerance. | `30-60` | `60` | `60-120` |
| `max_tracked_panes` | `256` | panes | How many panes the rate limiter remembers. Raise when fleets exceed the default and old panes are being evicted. | `64-128` | `256-512` | `512-2048` |
| `max_events_per_pane` | `64` | events | Number of rate-limit events kept per pane. Raise if short bursts are being forgotten too quickly. | `16-64` | `32-64` | `64-128` |
| `cost_tracker_max_panes` | `512` | panes | How many panes the cost tracker remembers. Raise when you run fleets larger than the default. | `128-256` | `512` | `1024-4096` |

Validation:
- `rate_limit_window_secs >= 10`
- `max_tracked_panes >= 32`
- `max_events_per_pane >= 8`

## `[tuning.audit]`

These keys live under `[tuning.audit]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `retention_days` | `90` | days | Audit log retention window. Lower it to save disk; raise it when you need a longer forensic history. | `30-60` | `60-90` | `90-180` |
| `approval_ttl_secs` | `900` | s | Time-to-live for one-time approval tokens. Lower it for tighter safety windows; raise it for slower human approval loops. | `300-900` | `900` | `900-1800` |
| `max_raw_query_rows` | `100` | rows | Result cap for raw audit queries. Raise it only if operators or automation routinely need wider raw slices. | `50-100` | `100-250` | `100-500` |
| `artifact_retention_days` | `30` | days | How long replay artifacts are retained. Lower it on disk-constrained machines. | `7-30` | `14-30` | `14-60` |
| `shadow_rollout_days` | `14` | days | Default evaluation window for shadow rollouts. Raise it when rollout signals are sparse or seasonal. | `7-14` | `14` | `14-30` |

Validation:
- `retention_days >= 1`
- `approval_ttl_secs >= 60`

## `[tuning.web]`

These keys live under `[tuning.web]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `default_host` | `"127.0.0.1"` | address | Bind address for the web server. Keep loopback unless you have a deliberate remote-access plan with matching transport security. | `"127.0.0.1"` | `"127.0.0.1"` | `"127.0.0.1"` |
| `default_port` | `8000` | TCP port | Default listen port. Change only to avoid collisions or to fit your deployment conventions. | `8000` or free port | `8000` or free port | `8000` or deployment port |
| `max_list_limit` | `500` | rows | Hard ceiling for list endpoints. Raise if automation genuinely needs larger pages; keep it bounded to avoid large API scans. | `100-250` | `250-500` | `500-2000` |
| `default_list_limit` | `50` | rows | Default page size for list endpoints. Raise it for bulk-oriented operators, lower it for lighter UI/API usage. | `25-50` | `50-100` | `100-250` |
| `max_request_body_bytes` | `65536` (`64 KiB`) | bytes | Maximum HTTP request body size. Raise only if mutation or import endpoints need larger payloads. | `32768-65536` | `65536-131072` | `131072-524288` |
| `stream_default_max_hz` | `50` | Hz | Default maximum SSE event rate. Lower it to reduce fan-out load on busy fleets. | `10-25` | `25-50` | `10-25` |
| `stream_max_max_hz` | `500` | Hz | Hard ceiling for SSE event rate. Keep this bounded to avoid a single client forcing expensive streams. | `50-100` | `100-250` | `100-250` |
| `stream_keepalive_secs` | `15` | s | Interval between SSE keep-alive frames. Raise on stable local networks; keep lower when intermediaries are aggressive about idle connections. | `10-15` | `15` | `15-30` |
| `stream_scan_limit` | `256` | rows per scan | Per-scan page size for streaming queries. Raise if clients must catch up across larger backlogs. | `64-128` | `128-256` | `256-512` |
| `stream_scan_max_pages` | `8` | pages | Maximum pages scanned per streaming query. Raise only if large event backlogs are normal and acceptable. | `4-8` | `8` | `8-16` |

Validation:
- `default_list_limit <= max_list_limit`
- `max_list_limit >= 10`
- `stream_keepalive_secs >= 1`

## `[tuning.workflows]`

These top-level workflow keys live under `[tuning.workflows]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `max_steps` | `32` | steps | Maximum number of steps allowed in a workflow descriptor. Raise only if real descriptors are hitting the ceiling. | `16-32` | `32` | `32-64` |
| `max_wait_timeout_ms` | `120000` | ms | Maximum allowed `wait-for` timeout per workflow step. Raise for slow external systems or human approval paths. | `30000-120000` | `60000-120000` | `120000-300000` |
| `max_sleep_ms` | `30000` | ms | Maximum allowed sleep duration per workflow step. Lower it to discourage time-based orchestration. | `5000-30000` | `10000-30000` | `10000-60000` |
| `max_text_len` | `8192` | bytes | Maximum text payload size per workflow step. Raise if real automation needs larger prompts or payloads. | `4096-8192` | `8192` | `8192-16384` |
| `max_match_len` | `1024` | bytes | Maximum length of workflow match patterns. Raise only if legitimate descriptors need larger patterns. | `512-1024` | `1024` | `1024-2048` |
| `swarm_learning_index_timeout_secs` | `30` | s | Timeout for swarm-learning index operations. Raise when indexing or remote dependencies are slow. | `15-30` | `30` | `30-60` |
| `claude_code_limits_cooldown_ms` | `600000` | ms | Cooldown used after Claude Code limit events. Raise for noisier limit signals; lower for faster retry loops. | `300000-600000` | `600000` | `600000-900000` |
| `session_start_context_cooldown_ms` | `600000` | ms | Cooldown between session-start context injections. Raise when large fleets create too much repeated context injection. | `300000-600000` | `600000` | `600000-900000` |

Validation:
- `max_steps >= 4`
- `max_wait_timeout_ms >= 1000`

## `[tuning.workflows.cass_session_start]`

These keys tune the session-start CASS lookup path.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `hint_limit` | `3` | hints | Number of hints injected into session-start context. Raise only if the extra context is consistently useful. | `2-3` | `3` | `3-5` |
| `timeout_secs` | `8` | s | Timeout for the session-start CASS query. Raise when the backing search path is slower. | `5-8` | `8` | `8-12` |
| `lookback_days` | `30` | days | History window searched for session-start context. Lower it for tighter recency, raise it for slower-moving projects. | `7-30` | `30` | `30-45` |
| `query_max_chars` | `180` | chars | Maximum size of the generated CASS query string. Raise only if the session-start summarizer is truncating meaningful context. | `120-180` | `180` | `180-240` |
| `hint_max_chars` | `160` | chars | Maximum size of each returned hint. Raise if useful context is being cut off too aggressively. | `120-160` | `160` | `160-220` |

Validation:
- No extra load-time validation beyond TOML typing.

## `[tuning.workflows.cass_on_error]`

These keys tune the on-error CASS lookup path.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `hint_limit` | `3` | hints | Number of hints injected after an error. Raise only if the error-recovery prompts clearly benefit from more context. | `2-3` | `3` | `3-5` |
| `timeout_secs` | `6` | s | Timeout for the error-path CASS query. Keep this tighter than session-start unless error recovery can tolerate more latency. | `4-6` | `6` | `6-10` |
| `lookback_days` | `30` | days | History window for error recovery lookups. Raise if relevant failure history spans longer than a month. | `7-30` | `30` | `30-45` |
| `query_max_chars` | `200` | chars | Maximum size of the error-path CASS query string. Raise only if queries are losing crucial error detail. | `120-200` | `200` | `200-260` |
| `hint_max_chars` | `180` | chars | Maximum size of each returned hint on the error path. | `120-180` | `180` | `180-240` |

Validation:
- No extra load-time validation beyond TOML typing.

## `[tuning.workflows.cass_auth]`

These keys tune the auth-related CASS lookup path.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `hint_limit` | `3` | hints | Number of auth-related hints injected into context. | `2-3` | `3` | `3-5` |
| `timeout_secs` | `8` | s | Timeout for auth-path CASS queries. Raise only if auth context lookups are slow but still valuable. | `5-8` | `8` | `8-12` |
| `lookback_days` | `30` | days | History window for auth context. | `7-30` | `30` | `30-45` |
| `query_max_chars` | `160` | chars | Maximum size of the auth CASS query string. | `120-160` | `160` | `160-220` |
| `hint_max_chars` | `140` | chars | Maximum size of each auth hint. | `100-140` | `140` | `140-200` |

Validation:
- No extra load-time validation beyond TOML typing.

## `[tuning.search]`

These keys live under `[tuning.search]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `default_limit` | `20` | rows | Default result limit for search queries. Raise for bulk-heavy automation; lower for interactive use. | `10-20` | `20-50` | `50-100` |
| `max_limit` | `1000` | rows | Hard ceiling for search result count. Raise only if automation genuinely needs larger result sets. | `100-500` | `500-1000` | `1000-5000` |
| `saved_search_limit` | `50` | rows | Default limit for saved-search list queries. | `20-50` | `50-100` | `100-250` |
| `cass_export_limit` | `1000` | rows | Maximum records exported through CASS export paths. Raise only for explicit offline analysis workflows. | `100-500` | `500-1000` | `1000-5000` |
| `tantivy_writer_memory_bytes` | `50000000` (`50 MB`) | bytes | Tantivy writer memory budget. Raise when indexing throughput matters more than RAM; lower it on constrained hosts. | `16000000-50000000` | `50000000-100000000` | `100000000-256000000` |

Validation:
- `default_limit <= max_limit`
- `max_limit >= 10`
- `tantivy_writer_memory_bytes >= 10485760` (`10 MiB`)

## `[tuning.wire_protocol]`

These keys live under `[tuning.wire_protocol]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `max_message_size` | `1048576` (`1 MiB`) | bytes | Distributed-mode message size ceiling. Raise only if real payloads exceed the default and both sides can absorb the larger frames. | `262144-1048576` | `1048576-4194304` | `1048576-8388608` |
| `max_sender_id_len` | `128` | bytes | Maximum sender identity length on the wire. Usually leave this alone unless your sender IDs are deliberately longer. | `64-128` | `128` | `128-256` |

Validation:
- `max_message_size >= 65536` (`64 KiB`)
- `max_message_size <= 67108864` (`64 MiB`)

## `[tuning.ipc]`

These keys live under `[tuning.ipc]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `max_message_size` | `131072` (`128 KiB`) | bytes | Local IPC message size ceiling. Raise if local RPC callers legitimately exchange larger payloads. | `65536-131072` | `131072-262144` | `262144-1048576` |
| `accept_poll_interval_ms` | `100` | ms | Poll interval for accepting IPC connections. Lower it for faster accept latency; raise it slightly if you want to trim idle CPU. | `50-100` | `25-100` | `10-50` |

Validation:
- `max_message_size >= 16384` (`16 KiB`)
- `accept_poll_interval_ms >= 10`

## `[tuning.wezterm]`

These keys live under `[tuning.wezterm]`.

| Field | Default | Unit | What it controls / when to change it | 10 panes | 50 panes | 200+ panes |
| --- | --- | --- | --- | --- | --- | --- |
| `timeout_secs` | `30` | s | Timeout for the WezTerm CLI command path. Raise if the backend is slow or remote; lower it if you want faster failure detection. | `10-30` | `30` | `30-60` |
| `retry_delay_ms` | `200` | ms | Delay between WezTerm CLI retries. Lower for faster retries; raise if transient backend failures need more breathing room. | `100-200` | `200` | `200-500` |
| `max_error_bytes` | `8192` (`8 KiB`) | bytes | Maximum retained error output from the WezTerm CLI. Raise only if backend diagnostics are being truncated too aggressively. | `4096-8192` | `8192` | `8192-16384` |
| `connect_timeout_ms` | `5000` | ms | Timeout for connecting to the mux socket. Raise when sockets are remote or under heavy load. | `1000-5000` | `5000` | `5000-15000` |
| `read_timeout_ms` | `5000` | ms | Timeout for mux socket reads. Raise if large responses or slow backends cause false timeouts. | `1000-5000` | `5000` | `5000-15000` |
| `write_timeout_ms` | `5000` | ms | Timeout for mux socket writes. Raise when the backend is slow to accept writes. | `1000-5000` | `5000` | `5000-15000` |

Validation:
- No extra load-time validation beyond TOML typing.

## Practical Tuning Order

If you are tuning a live deployment and do not know where to start, use this order:

1. `runtime` and `backpressure`: capture latency, batching, and early pressure warnings.
2. `patterns`: dedup and anchor context once you understand rule volume.
3. `policy` and `web`: API fan-out and fleet-control ceilings.
4. `search`: only after you have measured indexing pressure and query behavior.
5. `workflows` and nested CASS sections: only if automation descriptors or context injection are clearly the bottleneck.
6. `wezterm`, `wire_protocol`, and `ipc`: only when backend or transport diagnostics point there.

The default configuration is intentionally conservative. For most fleets, the right first move is to leave most keys alone and adjust only the small set of knobs tied to the subsystem that is already showing stress.
