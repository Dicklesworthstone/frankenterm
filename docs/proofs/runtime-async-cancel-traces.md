# runtime_async Cancel-Trace Equivalence Classes

**Bead:** `ft-tf6g3.6`
**Companions:** `crates/frankenterm-core/tests/loom_*.rs`,
`docs/runtime/mazurkiewicz-traces.md`

This document is the cancel-focused companion to
`docs/runtime/mazurkiewicz-traces.md`. The older catalog lists each
runtime_async primitive's general schedule classes. This file narrows the
claim to cancellation: which events are observable, which event pairs are
dependent, which Mazurkiewicz equivalence classes remain after commuting
independent events, and which class the primitive belongs to when a waiter,
receiver, producer, or observer is cancelled.

For each primitive, `D` is the symmetric dependence relation over observable
events. Event pairs not listed in `D` commute: swapping their order cannot
change the model's observable result. "Cancel" means the runtime_async
operation is abandoned at an await boundary, a receiving handle is dropped, or
an observer path stops before its next operation. The wrappers intentionally
observe `Cx` cancellation before entering the asupersync await. Mid-flight
cancellation is represented by the caller's select/drop boundary rather than by
hidden tokio-shaped wake semantics inside the primitive.

## oneshot

Observable events:

| Event | Meaning |
| --- | --- |
| `S` | `send(v)` linearizes. |
| `CS` | sender-side close/drop linearizes. |
| `CR` | receiver cancellation/drop linearizes. |
| `R` | `recv` linearizes and returns `Some(v)` or `None`. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `S, R` | delivery consumes the single value. |
| `CS, R` | sender close decides whether recv returns `None`. |
| `CR, S` | receiver cancellation decides whether send returns `Err(v)`. |
| `CR, R` | receiver cancellation and receive are mutually exclusive terminal observations. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| delivery before cancel | `S < R`, `CR` after terminal | exactly one `Some(v)` | `loom_oneshot_delivers_value_exactly_once`, `loom_oneshot_recv_idempotent_after_terminal` |
| receiver cancel before send | `CR < S` | send returns `Err(v)` and no panic | `loom_oneshot_send_after_receiver_drop_returns_err`, `loom_oneshot_send_during_receiver_drop_no_panic` |
| sender close before recv | `CS < R` | recv returns `None` | `loom_oneshot_sender_drop_observed_as_none` |
| send-close race | `S` and `CS` contend | exactly one terminal result, `Some(v)` or `None` | `loom_oneshot_send_close_race_one_outcome` |

Cancel class: receiver cancellation sequences before sender-side observable
success; after the channel reaches a terminal state, cancellation commutes with
all later sender-side or receiver-side observations.

## watch

Observable events:

| Event | Meaning |
| --- | --- |
| `W(v)` | `send(v)` linearizes and bumps the version. |
| `C` | sender close/drop linearizes. |
| `X` | reader cancellation between snapshots. |
| `R` | `snapshot()` linearizes. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `W(v), R` | a snapshot may or may not include the write. |
| `C, R` | a snapshot may or may not observe close. |
| `W(v), C` | the closed flag and value/version tuple share the same mutex state. |
| `X, R` | a cancelled reader suppresses later snapshots. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| pre-write cancel | `X < R`, no later snapshot | no synthesized value; no regression because no observation happens | `loom_watch_initial_value_visible_before_writes` |
| snapshot before cancel | `R < X` | observed value is initial or a sent value; version is bounded by sends | `loom_watch_preserves_version_monotonicity` |
| multi-write last-wins | `W(i)` serialize before `R` | value is one sent value, version equals completed sends | `loom_watch_concurrent_writers_no_torn_reads` |
| close sticky under cancel | `C < X` or `C < R` | once close is observed, later sends cannot clear it | `loom_watch_close_is_sticky_against_late_send` |

Cancel class: reader cancellation commutes with sender-side writes that no
cancelled reader observes. It sequences before receiver-side observation when
it suppresses a snapshot.

## mpsc

Observable events:

| Event | Meaning |
| --- | --- |
| `S(v)` | `send(v)` pushes a value or returns `Err(v)` after close. |
| `R` | `recv` drains one value or returns `None`. |
| `C` | close sets the channel terminal flag. |
| `X` | sender or receiver cancellation at an await boundary. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `S(v), R` | recv consumes queued values. |
| `S(v), C` | close before send changes `Ok(())` into `Err(v)`. |
| `R, C` | close after buffered sends must drain before `None`. |
| `X, S(v)` | cancelled sender suppresses enqueue. |
| `X, R` | cancelled receiver suppresses dequeue. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| sender cancel before enqueue | `X < S(v)` | no value is added; capacity is unchanged | declared in `loom_mpsc_cancel_trace_classes_are_declared` |
| enqueue before cancel | `S(v) < X` | value is delivered exactly once unless close wins before send | `loom_mpsc_preserves_capacity_and_delivery` |
| full-queue park then receive | `S(full)` parks, `R` frees slot | parked sender completes after wake | `loom_mpsc_full_queue_unblocks_after_recv` |
| close then send | `C < S(v)` | send returns `Err(v)` without panic | `loom_mpsc_send_after_close_returns_err` |
| close after buffered sends | `S* < C < R*` | buffered values drain before `None` | `loom_mpsc_close_drains_then_observes_none` |

Cancel class: send-side cancellation commutes with receiver operations when the
send has not enqueued. Receive-side cancellation sequences before dequeue if it
suppresses the receive; once a value is dequeued, cancellation cannot duplicate
or erase it.

## broadcast

Observable events:

| Event | Meaning |
| --- | --- |
| `S(v)` | send appends `(seq, v)` to the log. |
| `C` | close sets the terminal flag. |
| `R(n)` | receiver reads cursor `n`. |
| `X` | receiver cancellation between cursor reads. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `S(v), R(n)` | receiver visibility depends on whether seq `n` exists. |
| `C, R(n)` | close decides whether a missing cursor returns `None`. |
| `S(v), C` | close-before-send and send-before-close are distinct outcomes. |
| `X, R(n)` | cancellation suppresses later cursor reads. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| receiver cancel before next cursor | `X < R(n)` | no further cursor is observed | declared in `loom_broadcast_cancel_trace_classes_are_declared` |
| send before cursor | `S(v) < R(seq)` | every receiver maps the same seq to the same value | `loom_broadcast_preserves_total_order_across_receivers` |
| concurrent senders | `S(a)` and `S(b)` serialize | receivers agree on the selected total order | `loom_broadcast_concurrent_sends_serialize` |
| close before send | `C < S(v)` and `R(0)` | receiver observes `None` at cursor 0 | `loom_broadcast_close_before_send_visible_to_all` |
| send then close | `S* < C` | buffered messages drain before `None` | `loom_broadcast_close_after_sends_drains_then_closes` |

Cancel class: receiver cancellation commutes with sender log appends that the
cancelled receiver never reads. It sequences before the next cursor read when
it suppresses that observation.

## spsc_ring_buffer

Observable events:

| Event | Meaning |
| --- | --- |
| `P` | producer `try_send` advances `head`. |
| `Q` | consumer `try_recv` advances `tail`. |
| `C` | close sets the terminal flag. |
| `X` | producer or consumer cancellation between attempts. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `P, Q` | depth is `head - tail`. |
| `P, C` | close before send makes the send fail. |
| `X, P` | producer cancellation suppresses a head advance. |
| `X, Q` | consumer cancellation suppresses a tail advance. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| producer cancel before send | `X < P` | depth does not increase | declared in `loom_spsc_cancel_trace_classes_are_declared` |
| send before consumer cancel | `P < X` on consumer side | depth accounts for produced minus consumed | `loom_spsc_produced_equals_consumed_plus_depth` |
| close before future sends | `C < P` | future sends fail | `loom_spsc_close_prevents_future_sends` |
| send/receive race | `P` and `Q` commute only when queue is empty/full outcome is unchanged | depth stays between 0 and capacity | `loom_spsc_never_exceeds_capacity` |

Cancel class: cancellation between attempts commutes with the other side's
attempt when it does not change `head`, `tail`, or `closed`; otherwise it
sequences before the suppressed index advance.

## lockfree

Observable events:

| Event | Meaning |
| --- | --- |
| `A(i, v)` | sharded counter `fetch_add` on shard `i`. |
| `M(i, v)` | sharded max CAS loop on shard `i`. |
| `G` | aggregate read. |
| `X` | observer or updater cancellation between atomic operations. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `A(i, v), A(i, w)` | same-shard additions share one atomic counter. |
| `M(i, v), M(i, w)` | same-shard max updates share one CAS target. |
| `A(i, v), G` | aggregate read visibility depends on the add order. |
| `M(i, v), G` | aggregate read visibility depends on the CAS order. |
| `X, A(i, v)` | updater cancellation suppresses an atomic update. |
| `X, M(i, v)` | updater cancellation suppresses a CAS update. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| cancellation between independent shards | `X` commutes with operations on other shards | aggregate equals completed atomics only | `loom_counter_two_threads_separate_shards`, `loom_max_two_threads_separate_shards` |
| same-shard serialized add | `A(i, v)` and `A(i, w)` serialize | final sum includes both completed adds | `loom_counter_two_threads_same_shard` |
| same-shard max CAS race | `M(i, v)` and `M(i, w)` serialize/retry | final max is the largest completed observation | `loom_max_two_threads_same_shard`, `loom_max_cas_no_clobber` |
| mixed independent structures | counter and max touch disjoint atomics | cancellation of one path cannot corrupt the other | `loom_mixed_counter_and_max` |

Cancel class: cancellation between atomic operations commutes with independent
shards and independent structures. It sequences before a same-shard atomic if
it suppresses that operation.

## triple_buffer

Observable events:

| Event | Meaning |
| --- | --- |
| `P(v)` | writer publishes value `v` by CASing the packed state. |
| `A` | reader acquires the presented slot. |
| `F` | watchdog `force_recycle` swaps reader/presented slots. |
| `X` | reader, writer, or watchdog cancellation between retries. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `P(v), A` | reader may see seed or a completed publish. |
| `P(v), F` | both CAS the packed state byte. |
| `A, F` | both CAS the packed state byte. |
| `X, P(v)` | writer cancellation suppresses a publish. |
| `X, A` | reader cancellation suppresses an acquire. |
| `X, F` | watchdog cancellation suppresses a recycle. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| reader cancel before acquire | `X < A` | no reader observation; slot distinctness remains | declared in `loom_triple_buffer_cancel_trace_classes_are_declared` |
| publish before acquire | `P(v) < A` | reader sees seed or a coherent published value, never torn | `loom_writer_then_reader_two_thread`, `loom_slot_distinctness_under_concurrent_publish_acquire` |
| publish overrun | `P(v1) < P(v2)`, no intervening `A` | acquire observes latest; overrun is counted | `loom_two_publishes_one_acquire_observes_latest`, `loom_overruns_bounded_by_publishes` |
| force recycle race | `F` races with `P(v)` or `A` | every thread completes and slots stay distinct | `loom_force_recycle_preserves_slot_distinctness`, `loom_force_recycle_concurrent_with_publish_no_deadlock` |

Cancel class: cancellation between CAS retries commutes with slot locks on
distinct slots. It is dependent on the packed-state CAS it suppresses, because
that CAS selects the reader/writer/presented permutation.

## sync

This section covers `runtime_async::Mutex`, `runtime_async::RwLock`, and
`runtime_async::Semaphore`.

Observable events:

| Event | Meaning |
| --- | --- |
| `L` | lock/acquire linearizes. |
| `D` | guard or permit drop releases the primitive. |
| `T` | try-lock or availability snapshot observes current state. |
| `X` | waiting task cancellation before acquisition. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `L, D` | release enables later acquisition. |
| `L, L` | contended acquisitions serialize. |
| `L, T` | snapshot outcome depends on held/free state. |
| `D, T` | snapshot outcome depends on release visibility. |
| `X, L` | cancellation suppresses acquisition. |

Equivalence classes:

| Primitive | Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- | --- |
| Mutex | waiter cancel before lock | `X < L` | no critical-section entry | declared in `loom_sync_cancel_trace_classes_are_declared` |
| Mutex | sequenced acquisition | `L < D < L` | at most one holder, no lost updates | `loom_mutex_preserves_mutual_exclusion`, `loom_mutex_no_lost_updates_under_contention` |
| Mutex | drop wakes next | `D < L` | later acquirer observes prior write | `loom_mutex_drop_releases_for_next_acquirer` |
| RwLock | reader cancel before read | `X < L(read)` | no reader count increment | declared in `loom_sync_cancel_trace_classes_are_declared` |
| RwLock | concurrent readers | `L(read_i)` commute with `L(read_j)` | readers observe consistent value | `loom_rwlock_concurrent_readers_observe_consistent_value` |
| RwLock | writer exclusive | `L(write)` dependent with all readers/writers | no reader-writer overlap and no lost writes | `loom_rwlock_preserves_reader_writer_invariant`, `loom_rwlock_no_lost_writes_under_writer_contention` |
| Semaphore | waiter cancel before acquire | `X < L(acquire)` | permit count unchanged | declared in `loom_sync_cancel_trace_classes_are_declared` |
| Semaphore | drop-as-release | `L < D` | permit is restored | `loom_semaphore_drop_permit_restores_count`, `loom_semaphore_concurrent_balanced_no_leak` |
| Semaphore | release wakes blocked | `D < L(waiter)` | blocked acquirer completes in FIFO ticket order | `loom_semaphore_release_wakes_blocked_acquirer`, `loom_semaphore_honors_fifo_waiter_order` |

Cancel class: waiting-task cancellation sequences before acquisition if it
prevents a guard/permit from being created. After a guard or permit exists,
cancel-correctness is represented by ordinary drop, which is dependent with
later acquisition and snapshots.

## notify

`Notify` is present in the loom corpus even though the bead's minimum list did
not call it out separately.

Observable events:

| Event | Meaning |
| --- | --- |
| `N1` | `notify_one` linearizes. |
| `NA` | `notify_waiters` linearizes. |
| `W` | waiter observes a permit or epoch bump. |
| `X` | waiter cancellation before completion. |

Dependence relation `D`:

| Pair | Reason |
| --- | --- |
| `N1, W` | waiter may consume a permit. |
| `NA, W` | waiter may observe an epoch change. |
| `X, W` | cancellation suppresses waiter completion. |
| `N1, N1` | permit cap makes repeated notifications order-sensitive only until cap 1. |

Equivalence classes:

| Class | Partial order | Observable invariant | Loom assertion |
| --- | --- | --- | --- |
| waiter cancel before wake | `X < W` | waiter does not complete | declared in `loom_notify_cancel_trace_classes_are_declared` |
| notify_one before wait | `N1 < W` | one accumulated permit wakes one waiter | `loom_notify_one_pre_accumulates_permit` |
| wait before notify | `W` parks, then `N1` or `NA` | waiter completes exactly once | `loom_notify_one_wakes_one_waiter`, `loom_notify_waiters_wakes_currently_parked` |
| notify_waiters without waiters | `NA` before future `W` | no future permit is accumulated | `loom_notify_waiters_does_not_accumulate` |
| permit cap | `N1*` before `W` | cap remains one permit | `loom_notify_one_permit_caps_at_one` |

Cancel class: waiter cancellation commutes with producer notifications that no
remaining waiter observes. It sequences before waiter completion when it
suppresses that completion.

## Attestation contract

The machine-readable slot for this proof is
`docs/attestations/proofs/runtime-async-cancel-traces.json`. It records:

- the SHA-256 of this markdown document;
- the source files whose Loom tests enumerate the declared trace classes;
- the remote RCH commands used to build and run the supporting test binaries;
- the supporting test binary hashes when the remote proof lane emits them.

The attestation uses the existing `proofs/loom-runtime-async` release category
so no schema-category expansion is required for this cancel-focused companion.
