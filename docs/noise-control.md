# Noise Control: Dedupe, Cooldown, and Mute

wa suppresses noisy events through three mechanisms: deduplication,
notification cooldown, and explicit muting. All three are deterministic
and independent — dedupe controls event storage, cooldown controls
notification frequency, and muting is user-driven suppression.

## How Events Are Identified

Each event has a stable **identity key** computed from:

- `rule_id` — the pattern rule that fired
- `event_type` — the semantic category
- `pane_uuid` — stable pane identity (survives renames)
- Selected extracted fields — stable, non-secret metadata like
  `account_id`, `session_id`, or `model`

The key is deterministic: the same event in the same pane from the same
rule always produces the same key. This is what drives all three noise
control mechanisms.

## Deduplication

Dedupe collapses repeated identical events within a time window.

| Setting | Default | Purpose |
|---------|---------|---------|
| Window | 5 minutes | Time before a duplicate becomes a new event |
| Max capacity | 10,000 keys | Memory bound for tracked keys |

**How it works:**

1. An event arrives and its identity key is computed
2. If the key was seen within the window, the event is **suppressed** —
   it is not stored as a new event, and the suppressed count increments
3. If the window has expired, the event is treated as new and the
   suppressed count resets

Deduplication happens before cooldown. A suppressed event never reaches
the notification layer.

**When to adjust:**

- **Shorten the window** if you need to see every occurrence of a
  rapidly repeating event (e.g., monitoring a rate-limited API)
- **Lengthen the window** if the same event fires many times per minute
  and you only care about the first occurrence

## Notification Cooldown

Cooldown throttles how often notifications are sent for the same event
identity key. Unlike dedupe, cooldown does not affect event storage —
events are still recorded, but notifications are suppressed.

| Setting | Default | Purpose |
|---------|---------|---------|
| Cooldown | 30 seconds | Minimum interval between notifications for the same key |
| Max capacity | 10,000 keys | Memory bound for tracked keys |

**How it works:**

1. A notification would be sent for an event
2. If a notification for the same identity key was sent within the
   cooldown window, it is suppressed and the suppressed count increments
3. When cooldown expires, the next notification fires and includes the
   number of suppressed notifications since the last send

This prevents notification storms without losing events.

**When to adjust:**

- **Shorten cooldown** for high-urgency events where every notification
  matters (e.g., security alerts)
- **Lengthen cooldown** for informational events where a summary is
  sufficient

## Escalation

When the same event is repeatedly suppressed, escalation raises its
visibility:

| Trigger | Default | Action |
|---------|---------|--------|
| Suppression count | 3 within the dedupe window | Increase severity one level |
| Suppression age | 15 minutes of continuous suppression | Increase severity one level |

Severity levels: Info → Warning → Critical.

When escalation triggers:
- A single escalation notification is sent
- The notification includes total suppressed count and first/last seen
  timestamps
- Escalation state resets when the dedupe window expires

Escalation is deterministic — it fires based on counts and timestamps,
not heuristics.

## Muting

Muting is explicit user intent to stop notifications for an event
identity key. Muted events are still stored and visible in event
listings, but marked as `muted` and excluded from notifications.

### Mute an event

```bash
# Mute permanently
wa mute add evt:abc123

# Mute for a duration
wa mute add evt:abc123 --for 1h
wa mute add evt:abc123 --for 30m
```

The `evt:abc123` is the event identity key shown in event listings and
triage output.

### List active mutes

```bash
wa mute list
wa mute list --format json
```

### Remove a mute

```bash
wa mute remove evt:abc123
```

### Mute behavior

- **Scope:** Workspace-wide by default
- **Duration:** Indefinite unless `--for` is specified
- **Storage:** Muted events are stored with status `muted`
- **Reversibility:** `wa mute remove` restores normal notification
  behavior immediately
- **Ordering:** Mute decisions are applied before dedupe and cooldown

## Decision Flow

The notification gate processes events in this order:

```
Event arrives
  │
  ├─ Is identity key muted? → yes → store as "muted", no notification
  │
  ├─ Is identity key in dedupe window? → yes → suppress, increment count
  │                                     → if escalation threshold → escalate
  │
  ├─ Is identity key in cooldown window? → yes → suppress notification
  │
  └─ Send notification, reset cooldown timer
```

## Troubleshooting

### "Why didn't I get notified?"

Check each layer in order:

1. **Is the event muted?** Run `wa mute list` to see active mutes
2. **Is it being deduped?** The same event identity key within 5 minutes
   is suppressed by default. Check event timestamps.
3. **Is it in cooldown?** A notification for the same key within 30
   seconds is suppressed. Check notification timestamps.
4. **Is the notification channel configured?** Ensure your notification
   channel (webhook, etc.) is configured and reachable.

### "I'm getting too many notifications"

- **Lengthen the cooldown** for chatty events
- **Mute** specific event keys you don't need notifications for
- **Check dedupe** — if the dedupe window is too short, the same event
  generates many stored entries

### "A critical event was suppressed"

Escalation should catch this after 3 suppressions or 15 minutes. If
it didn't:
- Check the event's severity level — escalation only fires for events
  that reach the escalation threshold
- The dedupe window may have expired and reset the count between
  occurrences

### "How do I see muted events?"

Muted events are stored normally — they appear in event listings and
search results. Their status is `muted` rather than `unhandled`. Use
`wa events list` to see all events including muted ones.

## Configuration Summary

| Parameter | Default | Where to change |
|-----------|---------|----------------|
| Dedupe window | 5 min | `NotificationConfig` |
| Cooldown window | 30 s | `NotificationConfig` |
| Escalation count threshold | 3 | Escalation policy |
| Escalation age threshold | 15 min | Escalation policy |
| Max tracked keys (dedupe) | 10,000 | `EventDeduplicator` |
| Max tracked keys (cooldown) | 10,000 | `NotificationCooldown` |

All settings are deterministic and auditable. Changes take effect on the
next event cycle without restart.
