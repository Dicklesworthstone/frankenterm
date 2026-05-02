# Profile.apply daemon handler — integration runbook

**Bead:** `ft-4iz0q` (ft-b0g7g.cont.apply_spawn).
**In-core substrate:** `crates/frankenterm-core/src/robot_profile_handler.rs`
+ `crates/frankenterm-core/src/storage/profiles_applied_log_sql.rs`.

This doc captures the wired-pass handoff shape for the daemon
engineer: how to wire the in-core substrate (ApplyReceipt
content-hash + profiles_applied_log table + typed RPC envelope)
into the daemon's mux-spawning side without re-litigating the
idempotency contract or the storage schema.

## Substrate-pass commit table

| Slice | Commit | What ships |
|-------|--------|-----------|
| cc_1 slice 1 | `6a3653dc2` | `ApplyReceipt` struct + `compute_apply_content_hash()` (8 tests) |
| cc_2 slice | `830a4d1ef` | `ProfileApplySpawnRequest/Receipt/Outcome` typed RPC envelope (12 tests) |
| cc_1 slice 2 | `d030af643` | `profiles_applied_log` table + SQL primitives + migration v26 (11 tests) |

## Wired-pass integration shape

The daemon-side handler implements the full non-dry-run path:

```rust
use frankenterm_core::robot_profile_handler::{
    ApplyReceipt, compute_apply_content_hash,
};
use frankenterm_core::storage::profiles_applied_log_sql::{
    insert_apply_receipt, get_apply_receipt,
};
use frankenterm_core::robot_ntm_surface::{
    ProfileApplyData, ProfileApplyRequest,
};

fn handle_apply_non_dry_run(
    request: &ProfileApplyRequest,
    profile: &AgentProfile,
    storage: &Connection,
    mux: &mut MuxClient,
) -> Result<ProfileApplyData, ProfileHandlerError> {
    // Step 1: compute content_hash from the canonical inputs.
    let content_hash = compute_apply_content_hash(
        &profile.name,
        profile.updated_at_ms,
        request.count,
        &request.env_overrides,
    );

    // Step 2: idempotency lookup. If a receipt exists, return
    // its panes_spawned without re-spawning.
    if let Some(prior) = get_apply_receipt(storage, &content_hash)? {
        return Ok(ProfileApplyData {
            profile_name: prior.profile_name,
            panes_spawned: prior.panes_spawned,
            dry_run: false,
        });
    }

    // Step 3: spawn the requested count of panes via the mux
    // machinery. Each spawn binds:
    //   - profile.command (or profile.shell if command is None)
    //   - profile.env merged with request.env_overrides (overrides win)
    //   - profile.metadata.get("working_directory") as cwd
    //   - profile.metadata.get("layout_template") as layout hint
    let mut panes_spawned = Vec::with_capacity(request.count as usize);
    for _ in 0..request.count {
        let pane_id = mux.spawn(SpawnRequest {
            command: profile.command.clone()
                .or_else(|| profile.shell.clone()),
            env: merge_env(&profile.env, &request.env_overrides),
            cwd: profile.metadata.get("working_directory").cloned(),
            layout: profile.metadata.get("layout_template").cloned(),
        }).map_err(|err| ProfileHandlerError::SpawnFailed {
            reason: err.to_string(),
        })?;
        panes_spawned.push(pane_id);
    }

    // Step 4: persist the receipt for future idempotency.
    let receipt = ApplyReceipt {
        content_hash: content_hash.clone(),
        profile_name: profile.name.clone(),
        profile_updated_at_ms: profile.updated_at_ms,
        count: request.count,
        panes_spawned: panes_spawned.clone(),
        recorded_at_ms: now_ms(),
    };
    insert_apply_receipt(storage, &receipt)?;

    Ok(ProfileApplyData {
        profile_name: profile.name.clone(),
        panes_spawned,
        dry_run: false,
    })
}

fn merge_env(
    base: &HashMap<String, String>,
    overrides: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = base.clone();
    for (k, v) in overrides {
        merged.insert(k.clone(), v.clone());
    }
    merged
}
```

## IPC pathway (scope item 1 of ft-4iz0q)

Two real options the bead names:

**Option A** — reuse the existing daemon-IPC channel that
mutating CLI commands route through. Look at the spawn / send
/ wait_for sites for precedent — they connect to the running
daemon over the workspace socket. Recommended unless the
daemon's existing routing is bottlenecked.

**Option B** — add a new
`RobotMuxRequest::ApplyProfile { name, count, env_overrides }`
envelope to `crates/frankenterm-core/src/robot_envelope.rs`,
dispatch on the daemon side via the existing mux handler.
Cleaner separation but requires a new envelope variant +
serde + version bump.

The bead defers the choice to the engineer; both routes hit
the same `handle_apply_non_dry_run` function above.

## Error variant migration (scope item 4)

The deprecated `ProfileHandlerError::SpawnNotWired` variant was
removed in favour of typed `SpawnFailed { reason }` handling. The CLI
translator at `main.rs:23270` maps:

```rust
match err {
    ProfileHandlerError::SpawnFailed { reason } => {
        // The standalone handler reports robot.profile.spawn_failed.
        // Future daemon-side typed failures can split this into
        // storage / timeout / daemon-unreachable codes when those
        // reason kinds exist.
        eprintln!("{reason}");
    }
    // existing variants unchanged
}
```

## Conformance harness flip (scope item 5)

`tests/robot_family_conformance.rs::profile_stub_handler_passes_declared_invariants`
already drives the real handler (per ft-b0g7g closure at
`f5451a82e`). The wired-pass extension keeps `dry_run = true`
for the conformance tests since the harness doesn't own a
daemon; the new e2e test at
`crates/frankenterm/tests/robot_profile_apply_e2e.rs` (scope
item 5) boots a daemon, applies, and asserts both
`panes_spawned.len() == count` and the duplicate-apply
idempotency contract.

E2E sketch:

```rust
#[test]
fn duplicate_apply_returns_prior_panes_without_respawning() {
    let daemon = TestDaemon::start();
    let storage = daemon.storage();

    let req = ProfileApplyRequest {
        name: "dev".into(),
        count: 3,
        env_overrides: HashMap::new(),
        dry_run: false,
    };

    let first = daemon.client().apply_profile(&req).unwrap();
    assert_eq!(first.panes_spawned.len(), 3);

    // Re-apply with the SAME inputs → same content_hash →
    // get_apply_receipt hits → no re-spawn.
    let second = daemon.client().apply_profile(&req).unwrap();
    assert_eq!(second.panes_spawned, first.panes_spawned);

    // Verify the daemon's mux has only `count` panes (not 2×count).
    let mux_pane_count = daemon.client().mux_pane_count();
    assert_eq!(mux_pane_count, 3);
}
```

## Cross-references

- Substrate: [`crates/frankenterm-core/src/robot_profile_handler.rs`](../../crates/frankenterm-core/src/robot_profile_handler.rs) (ApplyReceipt + compute_apply_content_hash)
- Storage: [`crates/frankenterm-core/src/storage/profiles_applied_log_sql.rs`](../../crates/frankenterm-core/src/storage/profiles_applied_log_sql.rs) (insert/get/list/delete)
- Migration: [`crates/frankenterm-core/src/storage/migrations.rs`](../../crates/frankenterm-core/src/storage/migrations.rs) MIGRATIONS[25] (v26)
- Typed RPC envelope: cc_2's substrate at `830a4d1ef`
- Profile handler: `f5451a82e` (ft-b0g7g — list/show/validate/apply-dry-run)
- Bead: ft-4iz0q (parent — ft-b0g7g.cont.apply_spawn)
