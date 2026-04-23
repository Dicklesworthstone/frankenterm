# Policy-denial audit wiring matrix

**Bead:** ft-hg6io — wire `StorageHandle::record_policy_denial_audit`
(infra landed in ft-h90rh / 4595cc0e) into every `PolicyEngine::authorize`
call site so a denied or approval-gated action lands a row in
`policy_denied_audit` in addition to the existing `tracing::warn!`
emission from `mcp_authorize_mcp_mutation` (ft-6mmyp / 02988c16).

## Every authorize deny path in `crates/frankenterm-core/src/mcp_tools.rs`

Line numbers are drift-sensitive; re-grep `engine.authorize(&input)` /
`policy_engine.authorize(&input)` before editing.

| # | Call site (line) | Tool | Surface | Write? | Existing deny handling | Audit-write status |
|---|------------------|------|---------|--------|------------------------|--------------------|
| 1 | :194 (`mcp_authorize_mcp_mutation` helper) | `wa.tx_run`, `wa.tx_rollback`, `wa.mission_pause`, `wa.mission_resume`, `wa.mission_abort` | Mcp | ✓ | `tracing::warn!(target: "ft::security::policy", ...)` on both Deny and RequireApproval (ft-6mmyp) | **✗ missing** — helper returns `None` on Allow / `Some(err_envelope)` on Deny. No audit row. Fix: return a `GateOutcome` enum carrying a prebuilt `PolicyDeniedAuditRecord`, let callers (already in `runtime.block_on` scope) write it. |
| 2 | :1216 | `wa.get_text` | Mux | read | `MCP_ERR_POLICY` envelope on Deny | **✗ missing** — read-path deny isn't strictly a security breach, but recording it matches the invariant the bead sets and gives operators fleet-wide "who attempted reads they couldn't perform" forensics. |
| 3 | :1626 | `wa.search` | Mux | read | `MCP_ERR_POLICY` envelope on Deny | **✗ missing** — same rationale as :1216. |
| 4 | :2184 | `wa.send` | Mux | ✓ | Routes through `PolicyGatedInjector::send_text` which internally audits to `audit_actions` via `record_audit_action_redacted` on the success path; deny in dry-run branch emits envelope without audit | **partial** — success path audits to `audit_actions` (different table); deny path (and dry-run deny) does NOT write to `policy_denied_audit`. |
| 5 | :2435 | `wa.workflow_run` | Workflow | ✓ | Deny returns `MCP_ERR_POLICY` envelope; RequireApproval attaches to `ApprovalStore::attach_to_decision` and returns envelope | **partial** — has the deepest existing handling (approval-token issuance) but still doesn't record the deny itself. |
| 6 | :3385 | `wa.reserve` | Swarm | ✓ | `MCP_ERR_POLICY` envelope on Deny | **✗ missing** |
| 7 | :3536 | `wa.release` | Swarm | ✓ | `MCP_ERR_POLICY` envelope on Deny | **✗ missing** |
| 8 | :3795 | `wa.accounts_refresh` | Swarm | ✓ | `MCP_ERR_POLICY` envelope on Deny | **✗ missing** |

## Total deny-path surfaces that need audit writes

- 7 direct `authorize` sites in `mcp_tools.rs` (rows 2-8 above)
- 5 tools routed through `mcp_authorize_mcp_mutation` (row 1 above)

= **12 distinct deny paths**.

## Suggested wiring plan (sequence matters)

1. **Land pane 2's ft-eu0no** first. policy.rs has an uncommitted +81/-16
   diff refactoring `PolicyEngine.rate_limiter: RateLimiter` →
   `SharedRateLimiter`. Any change to policy.rs today will either collide
   or bundle their in-flight work.
2. **Add the policy.rs convenience wrapper** once policy.rs is clean:
   ```rust
   pub fn build_policy_denial_record(
       decision: &PolicyDecision,
       tool_name: &str,
       agent_id: Option<&str>,
       intent_hash: Option<&str>,
   ) -> Option<PolicyDeniedAuditRecord>
   ```
   Returns `None` for Allow, `Some` with the right `reason_code` /
   `decision` constants (from `PolicyDeniedAuditRecord::REASON_CODE_*`) for
   Deny / RequireApproval.
3. **Refactor `mcp_authorize_mcp_mutation`** to return a `GateOutcome`
   enum:
   ```rust
   enum GateOutcome {
       Allow,
       Denied { envelope: McpResult<Vec<Content>>, record: PolicyDeniedAuditRecord },
       RequireApproval { envelope: McpResult<Vec<Content>>, record: PolicyDeniedAuditRecord },
   }
   ```
   Each of the 5 callers (tx_run/rollback, mission_pause/resume/abort)
   already lives inside `runtime.block_on(async { ... })`, so the deny
   branch can `storage.record_policy_denial_audit(record).await.ok();`
   (error-ignoring to match the existing `tracing::warn!` fire-and-forget
   semantics).
4. **Wire each of the 7 direct authorize sites** (rows 2-8) to also
   build a `PolicyDeniedAuditRecord` on deny and write it. These are
   already inside async blocks so no runtime plumbing needed.
5. **Round-trip test** per-tool: seed a deny-by-default policy, invoke
   the tool, `SELECT FROM policy_denied_audit WHERE tool_name = ?` and
   assert the row shape.

## Why not in this 45-min slot

- Steps 2-4 require editing `policy.rs` + `mcp_tools.rs`, both contested.
- Step 3 is the sharpest collision: the `GateOutcome` refactor changes
  the helper signature, which ripples into the 5 callers in
  `mcp_tools.rs` — pane 2 is also editing those 5 handlers.
- Committing partial wiring under ft-hg6io would bundle pane 2's
  uncommitted `build_policy_engine_with_shared_rate_limiter` threading
  (~70 lines, including a 6× duplicated `let policy_rate_limiter = ...`
  cluster at ~:1535 that's an apparent editor glitch). Flagged in
  commit 810ad4c9 for pane 2 cleanup; doing the wiring on top would
  compound the mess.

## Follow-up beads to file once ft-eu0no lands

- **ft-\<A\>**: policy.rs `build_policy_denial_record` helper + unit test
- **ft-\<B\>**: refactor `mcp_authorize_mcp_mutation` to `GateOutcome` +
  wire 5 mutation callers. Round-trip integration test for one
  representative tool.
- **ft-\<C\>**: wire the 7 direct authorize sites (rows 2-8). Per-site
  deny-policy integration test.

Splitting this way keeps each bead under ~200 LoC so the implementer
can land them cleanly in sequence.
