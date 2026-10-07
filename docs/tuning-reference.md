# Tuning Reference

`TuningConfig` is loaded from the `[tuning]` section of `ft.toml`. Every key is optional. If you omit a key, `ft` keeps the hard-coded default from `crates/frankenterm-core/src/tuning_config.rs`.

This reference is for operators tuning live fleets. The ranges below are starting points, not promises. Change one subsystem at a time, run `ft config validate`, then watch capture lag, queue depth, SQLite lock timing, memory pressure, search latency, and workflow completion time before moving further.

Before you tune a live deployment:

- For blessed 10, 50, and 200+ pane starting profiles with exact commands, evidence citations, and escalation rules, start with `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md`.
- For current rollout posture, hardware-tier defaults, and fallback guidance, use `docs/resize-user-facing-release-tuning-guidance-wa-1u90p.8.5.md`.
- For incident triage and rollback flow, use `docs/operator-playbook.md`.
- For telemetry-driven auto-tuning modes, proof labels, and rollback requirements, use `docs/proposals/ft-luq3w-safe-auto-tuning-contract.md`.
- Treat this file as the knob-level reference, not the release-posture document.

## Auto-Tuning Enablement Gate

Telemetry-driven auto-tuning is not active by default. The library-level
`AutoTuneConfig::default()` keeps `enabled = false`, and the bounded candidate
engine starts in `observe` mode. That means a default process may record
telemetry and decision-shape proof, but it must not mutate live tuning knobs
unless an operator deliberately opts into a stronger mode.

Operator modes:

| Mode | Live mutation | Use |
| --- | --- | --- |
| `disabled` | No | Emergency off switch or normal default. Existing values stay pinned and no candidates are evaluated. |
| `observe` | No | Collect telemetry and emit would-have-tuned decision records with `would_apply=false`. |
| `canary` | Canary scope only | Try one registry-approved candidate on a bounded scope with explicit rollback metrics. |
| `exploration` | Temporary bounded scope | Evaluate one candidate while safety telemetry remains fresh and confidence thresholds pass. |
| `steady_state` | Yes | Keep a previously proven setting inside a narrow range. Do not enter from `observe` without canary/exploration evidence. |
| `rollback` | Yes, toward previous safe value only | Restore the last known safe value after regression, stale telemetry, or operator disable. |
| `cooldown` | No | Pause after rollback or missing telemetry until the unsafe reason clears. |

Enablement rules:

1. Start with `disabled` or `observe`.
2. Move to `canary` only after the decision log shows fresh telemetry, bounded
   candidate values, and a rollback metric for the selected knob.
3. Move to `exploration` or `steady_state` only after remote proof artifacts
   and retained decision records support the exact profile being enabled.
4. Pause by switching back to `observe` or `cooldown`; disable by setting the
   profile to `disabled` or `enabled = false`.
5. Roll back by entering `rollback`; the controller must restore the previous
   safe value and then enter `cooldown` before another candidate on that knob.

Reduced replay proof verifies logic, schemas, decision records, and rollback
behavior. It does not prove high-core/high-memory benefit. When RCH is healthy,
use this remote-reduced proof shape for the auto-tuning library slice:

```bash
RCH_NO_UPDATE_CHECK=1 \
RCH_EXTERNAL_TIMEOUT_ENABLED=false \
RCH_BUILD_TIMEOUT_SEC=3600 \
RCH_TEST_TIMEOUT_SEC=3600 \
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-luq3w-auto-tune-target \
  cargo test -p frankenterm-core --lib --no-default-features auto_tune -- --nocapture
```

If the local RCH wrapper recovery from `ft-tn6cw.1` is still required, replace
`rch` with the patched wrapper path and record that exact path in the Beads
comment and proof artifact. If RCH fails before Cargo starts, classify the
attempt as an infrastructure blocker, not as a source result.

High-scale proof requires all of the following:

- the same focused auto-tuning test path runs through remote Cargo on a
  target-class worker or host,
- retained `ft doctor --json` or equivalent hardware evidence proves the
  64-core / 256 GiB predicate for that run,
- replay or live artifacts include before/after latency, memory pressure,
  queue depth, dropped-work, and decision-log summaries,
- the proof report records `target_hardware`; otherwise use
  `skipped_not_proven`.

No README, Beads comment, release note, or operator runbook should claim
production or high-scale benefit from `local_reduced`, `remote_reduced`, or
`skipped_not_proven` evidence.

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
- `max_sender_id_len >= 32`
- `max_sender_id_len <= 4096`

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

## Durable Scrollback Writer (GUI and mux config)

These keys live in the GUI and mux config (`frankenterm.lua`, `wezterm.lua` or
`frankenterm.toml`), not in `ft.toml`. One `scrollback-durability` thread per
process stores every pane's evicted scrollback rows. The keys are
process-wide, and each new pane applies their current values.

| Key | Default | Range | What it controls |
| --- | --- | --- | --- |
| `scrollback_durability_queue_max_mb` | `64` | any | Byte budget for rows queued behind the writer, shared by all panes. Past it, a pane's oldest queued rows are dropped from durability and recorded as an explicit gap marker instead of throttling the pane (ft-yccm0.2.1.6; see `docs/resource-pressure-cockpit-contract.md`). |
| `scrollback_durability_commit_window_ms` | `250` | `0..=5000` | Commit window. A pane's queued rows wait up to this long after the first one, then go to the store in one transaction with one set of syncs (ft-yccm0.2.1.2). `0` commits every handoff at once. |
| `scrollback_durability_commit_window_mb` | `8` | `1..=256` | Closes a window early once this many MiB are queued. |
| `scrollback_durability_commit_idle_ms` | `25` | `0..=5000` | Closes a window early once its pane has queued no row for this long, so a burst commits as soon as it ends. |
| `scrollback_durability_manifest_publish_ms` | `1000` | `0..=60000` | Shortest interval between two manifest publications of one pane (ft-yccm0.2.1.2). Between them each window appends its rows under one sync. `0` publishes after every store batch. |

Other events close a window at once:

- a full window (65,536 rows, sixteen store batches);
- an overload gap, or a queue past half its budget;
- an explicit flush, such as a checkpoint;
- the pane closing.

For A/B runs, `FT_DURABILITY_COMMIT_WINDOW_MS` overrides the window, up to
5000 ms, and `FT_DURABILITY_MANIFEST_PUBLISH_MS` the publication interval, up
to 60000 ms.

**Publication.** A window's rows are durable once its single sync returns.
The manifest that describes them is published at most once per interval,
and also on pane close, on an explicit flush and before a snapshot. A
publication writes one cumulative WAL naming every row since the last
manifest, prunes what retention evicted, and replaces the manifest: about
eight syncs, once per interval rather than once per batch. After a crash,
reopen adopts every unpublished row that opens at its exact location and
cuts a torn or reordered remainder. A process's first window, a key
rotation, or a retention change runs as one store batch with its own WAL
and manifest, so the nonce segment its rows need is published before a
tail depends on it.

**Tradeoff.**

- **Longer windows.** Rows from many parse batches share one transaction,
  which cuts syncs, renames and manifest publications per row.
- **Exposure.** Rows sit in memory, not on disk, until their window closes. A
  crash loses the queued rows: about one window of output when the writer
  keeps up, up to the queue budget when it is behind.
- **Overload.** Under a flood, the batch limit closes windows long before the
  window age does, so the window mainly saves IO for interactive output and
  moderate streams.

## Pane Output Ring (GUI and mux config)

Each pane's reader thread hands PTY output to its parser thread through a
ring of preallocated slots (ft-yccm0.3.1.1). It replaced an AF_UNIX
socketpair, so moving bytes between the two threads costs no syscall and no
kernel copy. The parser parses each slot in place. Each new pane applies the
current values.

| Key | Default | Range | What it controls |
| --- | --- | --- | --- |
| `mux_output_ring_slots` | `8` | `2..=1024` | Slots per pane. When every slot holds unparsed output, the reader parks and stops reading the PTY, so the kernel throttles the child. |
| `mux_output_ring_slot_bytes` | `65536` | `4096..=16777216` | Bytes per slot. One delivery fills as many slots as it needs; a slot is published as soon as its delivery ends, so small output is not held back. |

On Unix the reader gathers PTY output into batches before it hands them over
(ft-yccm0.3.1.2). A macOS PTY master returns about 1 KiB per `read` under
load (Ghostty observes the same in `Exec.zig`), and every batch costs a ring
publication, a parser wake and terminal-model work. So:

- A first read below `mux_output_gather_min_bytes` is published at once, so
  keypress echo never waits.
- From that size on, the reader keeps reading what is already buffered:
  `mux_output_gather_spin_checks` immediate checks, then polls of at most
  `mux_output_gather_poll_us` each.
- It publishes at a full slot, EOF, an idle parser (the parser interrupts the
  poll before it sleeps), or after `mux_output_gather_max_wait_us` in total.

The PTY descriptor stays blocking, because it shares its open file description
with the input writer; the reader polls before every read after the first.

| Key | Default | Range | What it controls |
| --- | --- | --- | --- |
| `mux_output_gather_min_bytes` | `1024` | `0..=1048576` | Smallest first read that starts a gather. Below it, output is published at once. |
| `mux_output_gather_spin_checks` | `16` | `0..=1024` | Immediate readiness checks before the reader polls with a timeout. |
| `mux_output_gather_poll_us` | `1000` | `1..=100000` | Longest single poll while gathering, in microseconds. |
| `mux_output_gather_max_wait_us` | `3000` | `0..=100000` | Longest time one batch may gather, in microseconds. `0` turns gathering off. |

**Tradeoff.** Slots times slot bytes bounds the output buffered between
reader and parser, about 512 KiB per pane by default. The slots are
preallocated per pane; pages the OS has not touched cost no memory until
output fills them. A larger ring absorbs longer bursts while the parser is
busy, at the price of that memory per pane.

## GUI Performance Advisories (`FT-TUNE` codes)

These advisories cover the GUI config (`frankenterm.lua`, `wezterm.lua` or `frankenterm.toml`), not `ft.toml`. Each one names a setting that costs throughput or responsiveness without any other sign. The codes are stable and are never reused.

Where they appear:

- **GUI startup log.** The GUI logs one `warn` line per advisory, in the form `FT-TUNE-000N <setting>: <what it costs> Fix: <change> (<this page>)`. It judges `max_fps` against the fastest screen it sees.
- **`ft doctor`.** Each advisory is a `performance config (<setting>)` warning row. In `--json`, the row carries a `code` field. The `config_tuning` block lists the inputs, where they came from, and every advisory with its `code`, `subject`, `detail`, `remediation` and `docs`.

Where `ft doctor` gets the settings:

- **From a running GUI.** A GUI publishes the settings it actually evaluated, and its fastest display, to `gui-tuning-<pid>.json` in the runtime directory. It republishes on config reload. Doctor trusts that file only while the same GUI's resource snapshot is fresh.
- **From the config file.** With no running GUI, doctor reads the first config file the GUI would load, as text, and never executes it. It reads `name = value` assignments. Integer values may be products such as `512 * 1024`. An assignment that shares a line with an `if` is not read. A computed value, for example from a function call, is not guessed.
- **Installed GUI version.** On macOS, the GUI version also comes from an installed `FrankenTerm.app` bundle.

The rules are implemented in `frankenterm/config/src/tuning.rs`.

### FT-TUNE-0001

**`mux_output_parser_buffer_size` above 128 KiB**

- **Trigger:** `mux_output_parser_buffer_size` is larger than its 128 KiB (`131072`) default.
- **Cost:**
  - Each pty read takes up to this many bytes, and the parsed batch is applied while the pane's terminal lock is held. A larger buffer means longer lock holds, and paint, mouse and resize all wait behind them.
  - Output that backed up during any stall arrives in bursts of this size.
  - A 512 KiB buffer makes every such burst four times the default.
- **Fix:** remove the setting, or set it to `131072` or less.
- **Tradeoff:** a larger buffer trims per-read overhead on a flood, but the extra lock time lands on the GUI thread.

### FT-TUNE-0002

**`max_fps` below the display's refresh rate**

- **Trigger:** `max_fps` is below the refresh rate of the fastest screen the GUI reported.
  - Without a running GUI, doctor cannot see the display and compares against 60 Hz, the `max_fps` default.
  - The default `60` on a 120 Hz ProMotion display also trips this rule.
- **Cost:** repaints are capped at `max_fps`, so output and typing echo reach the screen at most that many times a second. A 30 fps cap on a 60 Hz display halves the visible frame rate.
- **Fix:** set `max_fps` to the refresh rate named in the advisory, for example `max_fps = 120`.
- **Tradeoff:** a lower cap can save GPU work on battery. Make that choice deliberately, not as a leftover setting.

### FT-TUNE-0003

**Very large scrollback with tiering disabled**

- **Trigger:** `scrollback_tiered_enabled = false` and `scrollback_lines` above 10,000.
- **Cost:**
  - Without tiering, every scrollback line of every pane stays in memory as a full line.
  - A resize rewraps every one of those lines, so memory and resize time grow with `scrollback_lines` times the pane count.
- **Fix:** set `scrollback_tiered_enabled = true` (the default), or lower `scrollback_lines` to 10,000 or less.
  - With tiering, the newest `scrollback_hot_lines` lines (default 1,000) stay hot and older lines move to the warm and cold tiers.
- **Tradeoff:** an untiered buffer avoids tier transitions entirely. The 10,000-line threshold is a heuristic, about three times the 3,500-line default.

### FT-TUNE-0004

**A front end that renders on the CPU**

- **Trigger:** either of these, on every platform:
  - `front_end = "Software"`, a software (CPU) OpenGL renderer;
  - `front_end = "WebGpu"` with `webgpu_force_fallback_adapter = true`, which makes wgpu use its fallback adapter, generally a software implementation.
- **Cost:** every frame is drawn on the CPU, which competes with the parser and the shell for the same cores.
- **Fix:** remove `front_end` to use the default GPU renderer (OpenGL), or set `front_end = "WebGpu"`. Set `webgpu_force_fallback_adapter = false` (the default).
- **Tradeoff:** a software renderer is a workaround for broken GPU drivers. Keep it only while that problem exists.
- **Platform rows:** none yet. When a measurement shows a GPU front end slower on a given platform, add a platform row in `tuning.rs` and here. The Metal default switch is ft-yccm0.4.8.

### FT-TUNE-0005

**The GUI is older than `ft`**

- **Trigger:** the GUI's release is older than the `ft` that runs doctor, compared as semantic versions (`0.15.6-rc.53` is older than `0.15.21`). The GUI version comes from the running GUI if one published it, otherwise from an installed `FrankenTerm.app` bundle.
- **Cost:** the GUI runs without every change released after its version, including ingest, rendering and durability fixes.
- **Fix:** install `FrankenTerm.app` from the same release as `ft` (`install.sh --version <ft's version>`), then restart FrankenTerm.
- **Tradeoff:** none. Two builds of the same version that differ only by commit are not ordered, and are not reported.
