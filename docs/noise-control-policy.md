# Noise Control Policy (Dedupe, Cooldown, Escalation, Mute)

## Purpose
This document defines deterministic rules for suppressing noisy events without losing observability. It specifies:
- How to compute event identity keys
- How dedupe and cooldown windows behave
- When to escalate repeated events
- How muting works and how it is scoped

This is a design spec to make the behavior implementable without ad-hoc special cases.

## Terms
- Detection: A pattern match from the pattern engine.
- Event: A stored detection with metadata and lifecycle state.
- Event identity key: A stable key used for dedupe, cooldown, and muting decisions.
- Dedupe window: Time window where identical events collapse into one occurrence.
- Cooldown window: Time window where notifications are suppressed after a send.
- Escalation: A policy that increases severity or visibility after repeated suppression.
- Mute: An explicit user action to suppress notifications for a specific event identity key.

## Event Identity Key
The event identity key must be stable, deterministic, and free of secrets. It must group events that are semantically the same and separate events that are meaningfully different.

Key components:
- `rule_id` (stable rule name)
- `event_type` (semantic category)
- `pane_uuid` (stable pane identity across renames and title changes)
- Selected extracted fields (stable, non-secret, minimal cardinality)

Selected extracted fields should only include values that help distinguish distinct event streams. Examples:
- `account_id`, `session_id`, `model`, `host`, `domain`, `auth_provider`
- Avoid raw tokens, URLs with query params, cookies, device codes, or full prompts

Normalization rules:
- Keys are lowercased and sorted
- Values are trimmed and normalized (e.g., collapse whitespace)
- If any field might include secrets, hash it before inclusion

Suggested canonical form:
```text
key_parts = [
  rule_id,
  event_type,
  pane_uuid,
  sorted(selected_extracted_kv_pairs)
]
identity_key = sha256(join("|", key_parts))
```

If hashing is not available in a given layer, a plain joined string is acceptable as long as it avoids secrets.

## Dedupe Window
Dedupe suppresses repeated identical events inside a time window.

Design rules:
- Dedupe is keyed by the event identity key.
- Default window: 5 minutes.
- If a duplicate arrives inside the window, the event is suppressed and the suppressed count increments.
- If the window expires, the next occurrence is treated as a new event and resets the suppressed count.

The dedupe window is separate from cooldown. Dedupe affects whether an event is emitted or stored; cooldown only affects notifications.

## Cooldown Window
Cooldown suppresses repeated notifications after a notification is sent.

Design rules:
- Cooldown is keyed by the event identity key.
- Default window: 30 seconds.
- If a notification is suppressed during cooldown, the suppressed count increments.
- When cooldown expires, the next notification is sent and includes the number of suppressed notifications since the last send.

Cooldown should not prevent event storage. It only throttles user-facing outputs (webhook, desktop, TUI notifications).

## Escalation Policy
Escalation provides a deterministic path for repeated suppression to surface as higher severity or explicit summaries.

Recommended thresholds:
- Escalate after `N` suppressed events within the dedupe window (default N=3).
- Escalate if a key has been suppressed for longer than `T` minutes (default T=15).

Escalation actions:
- Increase severity by one level (Info -> Warning -> Critical).
- Emit a single escalation notification that includes total suppressed count and first/last seen timestamps.
- Reset escalation state when the dedupe window expires.

Escalation should be deterministic and tied to counts and timestamps, not heuristics.

## Muting
Muting is explicit user intent to suppress notifications for an event identity key.

Design rules:
- Mutes apply at notification time and triage listing time.
- Muted events are still stored; they are marked as handled with status `muted`.
- Default scope: workspace-wide. Optional global scope may be added later.
- Mutes can be indefinite or have an optional TTL.
- Muting should be listable and reversible.

Suggested storage model:
- `event_mutes` table keyed by identity key, scope, created_at, expires_at
- Mute decisions are applied before dedupe/cooldown decisions

Suggested UX surfaces:
- `wa mute add <rule_id> [--pane <id>] [--ttl <duration>]`
- `wa mute list`
- `wa mute remove <key>`
- TUI: mark selected event muted; provide "show muted" filter

## Implementation Crosswalk
Existing components that align with this policy:
- `Detection::dedup_key` in `crates/wa-core/src/patterns.rs`
- `EventDeduplicator` and `NotificationCooldown` in `crates/wa-core/src/events.rs`
- `NotificationConfig` in `crates/wa-core/src/config.rs`
- `events.dedupe_key` column in `crates/wa-core/src/storage.rs`
- TUI mute action in `crates/wa-core/src/tui/query.rs`

Recommended adjustments to align with this spec:
- Base dedupe and cooldown keys on `pane_uuid` instead of `pane_id`.
- Extend the key to include selected extracted fields (or a hash of them).
- Add persistent mute storage instead of muting only a single event row.
- Add escalation tracking in the event gate or storage layer.

## Non-Goals
- Heuristic or ML-based suppression.
- Loss of stored events due to cooldown.
- Muting that deletes historical data.

