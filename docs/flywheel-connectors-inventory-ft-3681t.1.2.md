# Flywheel Connectors — Exhaustive Capability/Security Inventory

> **Bead**: ft-3681t.1.2
> **Author**: PinkForge (claude-code, opus-4.6)
> **Date**: 2026-03-11
> **Source**: /dp/flywheel_connectors (Rust, 25+ core crates, 90+ connectors)

---

## Executive Summary

flywheel_connectors implements the **Flywheel Connector Protocol (FCP v2)**, a specification for secure, distributed AI assistant operations across personal device meshes. It consists of 25+ core infrastructure crates, 90+ production connectors (Twitter, Gmail, GitHub, Stripe, etc.), and extensive supply-chain security enforcement.

**Key assessment**: FCP v2 is a sophisticated security-first connector framework. FrankenTerm should adopt the connector governance model (manifest-based capability control, sandbox enforcement, audit chains) while adapting the transport layer (RaptorQ/Tailscale are mesh-specific).

## Code-Grounded FrankenTerm Baseline (2026-03-11 delta)

FrankenTerm already contains a meaningful in-repo connector-fabric skeleton, so
this inventory needs to drive convergence work rather than describe a purely
hypothetical future system.

- `crates/frankenterm-core/src/connector_host_runtime.rs`: deterministic host
  runtime config, lifecycle phases, sandbox decisions, capability envelopes,
  runtime budgets, and auditable operation envelopes.
- `crates/frankenterm-core/src/connector_registry.rs` +
  `crates/frankenterm-core/src/connector_bundles.rs`: signed bundle/registry
  trust model, ingestion pipeline, and tiered packaging seams.
- `crates/frankenterm-core/src/connector_sdk.rs`,
  `crates/frankenterm-core/src/connector_event_model.rs`,
  `crates/frankenterm-core/src/connector_inbound_bridge.rs`,
  `crates/frankenterm-core/src/connector_outbound_bridge.rs`: execution SDK and
  bridge/event contracts between ft and external connectors.
- `crates/frankenterm-core/src/connector_credential_broker.rs` +
  `crates/frankenterm-core/src/connector_data_classification.rs`: secret
  brokering, sensitivity tiers, provenance, and redaction constraints.
- `crates/frankenterm-core/src/connector_lifecycle.rs`,
  `crates/frankenterm-core/src/connector_reliability.rs`,
  `crates/frankenterm-core/src/canary_rollout_controller.rs`: rollout,
  health, rollback, and reliability controls.
- `crates/frankenterm-core/src/connector_mesh.rs`,
  `crates/frankenterm-core/src/distributed.rs`,
  `crates/frankenterm-core/src/policy.rs`: multi-host routing/federation seams
  are already threaded into the policy engine and safety config.
- `crates/frankenterm-core/src/policy_audit_chain.rs`: tamper-evident audit
  chain primitives that are the closest current in-repo analogue to FCP audit
  events and decision receipts.

The practical takeaway is that FrankenTerm already has most of the structural
seams needed to absorb FCP governance concepts. The hard remaining work is
making those seams normative and interoperable rather than parallel prototypes.

## Source Implementation Anchors (/dp/flywheel_connectors)

| Source Anchor | Responsibility | FrankenTerm Relevance |
|---|---|---|
| `crates/fcp-manifest/src/lib.rs` | Strict TOML manifest parsing, interface hash computation, capability/network validation | Primary source for connector declaration, interface stability, and fail-closed package admission. |
| `crates/fcp-registry/src/lib.rs` | Registry verification, endorsement, supply-chain validation | Direct precedent for signed bundle trust and connector admission controls. |
| `crates/fcp-sandbox/src/lib.rs` | OS sandbox profiles, egress policy, deny-by-default network controls | Main source for how strict connector isolation should behave under failure or ambiguity. |
| `crates/fcp-mesh/src/lib.rs` | Mesh federation, node identity, routing, leases, admission | Relevant for distributed FrankenTerm only; should not dictate local-first architecture. |
| `crates/fcp-audit/src/lib.rs` + `crates/fcp-core/src/audit.rs` | Audit-event and decision-receipt data structures, hash-linked evidence semantics | Best upstream precedent for strengthening ft policy/connector audit continuity. |
| `FCP_CDDL_V2.cddl` | Canonical type-level contract for capabilities, approvals, invoke/simulate/subscribe, audit events | Useful for mechanical schema alignment and validating connector contract completeness. |
| `FCP_Specification_V3.md` | Normative semantics for authority, zones, provenance, durability, host/runtime behavior | Source of the execution/security invariants to preserve even when transport choices differ. |

---

## 1. Host Runtime

### Architecture
- **fcp-host**: Node gateway/orchestrator supervising connector binaries in sandboxes
- Connectors are standalone binaries spawned in isolated processes
- Host delegates enforcement to MeshNode + policy engine
- Admin state snapshots for query/simulation/restoration

### Connector Trait (Core Interface)
```rust
#[async_trait]
pub trait FcpConnector: Send + Sync {
    fn id(&self) -> &ConnectorId;
    async fn configure(&mut self, config: Value) -> FcpResult<()>;
    async fn handshake(&mut self, req: HandshakeRequest) -> FcpResult<HandshakeResponse>;
    async fn health(&self) -> HealthSnapshot;
    async fn invoke(&self, req: InvokeRequest) -> FcpResult<InvokeResponse>;
    async fn simulate(&self, req: SimulateRequest) -> FcpResult<SimulateResponse>;
    async fn subscribe(&self, req: SubscribeRequest) -> FcpResult<SubscribeResponse>;
    async fn unsubscribe(&self, req: UnsubscribeRequest) -> FcpResult<()>;
}
```

### Archetype Traits
- **RequestResponse**: REST API, GraphQL
- **Streaming**: WebSocket, SSE
- **Bidirectional**: Streaming + publish
- **Polling**: IMAP, RSS (pull-based)
- **Webhook**: GitHub, Stripe (push-based)

---

## 2. Protocol Model

### Wire Protocol (FCPS/FCPC)
- **FCPS** (Streaming Frame Protocol): Symbol-based with RaptorQ fountain codes
- **FCPC** (Control Plane): Small control messages (handshake, ack)

### SymbolEnvelope (Atomic Unit)
- `object_id`: Content address (keyed BLAKE3-256)
- `esi`: Encoding Symbol ID (position in fountain code)
- `k`: Total source symbols
- `data`: Payload (typically 1024 bytes)
- `zone_id`: Zone for encryption key selection
- `epoch_id`: Logical time binding
- `auth_tag`: AEAD authentication (16 bytes)

### Request-Response Messages
- **Handshake**: Protocol version, zone, host pubkey, nonce, capabilities negotiation
- **Invoke**: Operation ID, input params, context, holder_proof, correlation_id
- **Simulate**: Preflight check without side effects
- **Subscribe/Unsubscribe**: Event topic subscription with replay buffer

### Event Capabilities
Streaming, Replay, Acknowledgment, Buffering

---

## 3. Registry/Signature Flow

### Connector Manifest (TOML)
Sections: `[manifest]`, `[connector]`, `[zones]`, `[capabilities]`, `[provides.operations]`, `[sandbox]`, `[rate_limits]`, `[signatures]`, `[supply_chain]`, `[policy]`

### Verification Chain
1. Ed25519 signatures over canonical CBOR manifest
2. Publisher threshold (multi-sig)
3. Registry endorsement
4. Capability ceiling enforcement
5. Transparency log (Merkle proof)
6. SLSA level attestation
7. Supply chain evidence validation

### Content Addressing
- **ObjectId**: Keyed BLAKE3-256 binding content to zone + schema
- Privacy: Keyed hash prevents dictionary attacks
- Stability: Remains stable across zone_key rotations

---

## 4. Sandbox Zones

### Zone Architecture
- **ZoneId**: Namespace for cryptographic isolation (e.g., `z:work`, `z:owner`, `z:private`)
- Zone-scoped encryption keys (ChaCha20-Poly1305)
- Tailscale ACL tags mapped to zones

### Sandbox Profiles
| Profile | Network | Description |
|---------|---------|-------------|
| strict | No direct, all via egress proxy | Highest isolation |
| moderate | Restricted, proxy for untrusted | Balanced |
| permissive | Direct access allowed | Lowest isolation |

### OS-Level Enforcement
- **Linux**: seccomp-bpf + namespaces + Landlock (5.13+)
- **macOS**: Seatbelt profiles (sandbox-exec)
- **Windows**: AppContainer + job objects

### Egress Guard
- Deny-by-default network enforcement
- Per-operation host/port allow lists
- DNS restrictions (deny localhost, deny private ranges, require SNI)
- Timeout enforcement (connect, total, max response bytes)

---

## 5. Capability Governance

### Capability Token (FCT)
- COSE-signed JWT with fcp2_claims
- CapabilityId: Canonical identifier (lowercase ASCII, ≤128 bytes)
- CapabilityGrant: Scoped permission with constraints

### Constraints
- Credential allow lists
- Connector allow lists
- Zone scope restrictions
- Operation allow lists
- Network allow lists
- Rate limit overrides
- Approval requirements

### Holder Proof (Anti-Replay)
- When capability has `holder_node` binding, request MUST include signature
- Prevents replay by non-holder nodes

### Chain of Authority
```
Owner Key → Zone Keys → Capability Objects → Operations
```

---

## 6. Mesh Federation

### MeshNode Architecture
- **fcp-mesh**: Node orchestration, routing, admission, gossip, leases
- Identity: Tailscale node ID + Ed25519 signing key
- Transport priority: Direct > relay
- Per-peer budgets, anti-amplification

### Key Types
- `MeshIdentity`: node_id, owner_pubkey, node_sig_pubkey, node_enc_pubkey, node_iss_pubkey
- `NodeKeyAttestation`: Owner-signed attestation with device posture, nonce, expiry
- `DevicePostureAttestation`: TPM quote, Secure Enclave, Android keystore

### Distributed Leases
- Consensus-based grant allocation (k-of-n quorum)
- Short-lived, refreshed periodically
- Revocation + rollback atomicity

---

## 7. Connector Lifecycle

### State Machine
```
Pending → Installing → Canary ─(health OK)─→ Production
                         ↓                        ↓
                    RolledBack ←────(health fail)──┘
                         ↓
                      Disabled → Uninstalled
```

### Install Flow
1. Fetch manifest + binary from registry
2. Signature check, manifest validation, supply-chain attestation
3. Store in zone-scoped persistent storage
4. Parse manifest, set up rate limits, sandbox profile
5. Create runtime, apply OS sandbox, bind to zone keys
6. Exchange credentials, agree on capabilities
7. Self-check before entering canary

### Canary Promotion
- Health-based auto-promotion (configurable threshold)
- Manual promotion always available
- Auto-rollback on health degradation

### Health Monitoring
- `HealthSnapshot`: State (Healthy/Degraded/Failed), metrics, timestamp
- `SelfCheckReport`: Read-only connector checks
- `LivenessResponse`: Basic connectivity ping

---

## 8. Audit Chain Behavior

### Audit Event (Normative)
- Hash-linked chain (`prev` field points to previous event ObjectId)
- Monotonic sequence numbers (`seq`, O(1) freshness check)
- Correlation ID + W3C Trace Context

### Required Audit Events
| Event Type | Trigger |
|-----------|---------|
| `secret.access` | Every credential access |
| `capability.invoke` | Every capability use |
| `elevation.granted` | Approval granted |
| `declassification.granted` | Cross-zone data movement |
| `zone.transition` | Cross-zone operations |
| `revocation.issued` | Revocation events |
| `security.violation` | Access denials |
| `audit.fork_detected` | Chain integrity violation |

### Tamper Detection
- Hash-linked chain, quorum signatures on heads (Byzantine resilience)
- Zone checkpoints as GC roots
- Decision receipts for explainability (reason_code, evidence, explanation)

---

## 9. Key Data Types

### Core Identifiers
| Type | Example | Purpose |
|------|---------|---------|
| `ConnectorId` | "fcp.twitter" | Connector identity |
| `CapabilityId` | "twitter.write" | Capability identity |
| `OperationId` | "twitter.tweet.create" | Operation identity |
| `ZoneId` | "z:work" | Zone namespace |
| `ObjectId` | [32 bytes] | Content address (keyed BLAKE3) |
| `EpochId` | u64 | Logical time unit |
| `SecretId` | UUID | Secret reference |
| `CredentialId` | UUID | Credential reference |

### Credential Model
- `CredentialObject`: References SecretId, specifies application method
- `CredentialApplication`: Bearer, Basic, header, query param, TLS cert, SSH key, DB connection
- **Secretless egress**: Connectors reference CredentialId; host injects at boundary

### Secret Sharing (Threshold)
- k-of-n Shamir's Secret Sharing (ShamirGf256)
- Per-node HPKE-sealed wrapped shares
- Rotation policy with generation tracking
- Zeroize: All secret bytes wiped after use

### Provenance Tracking
- `Provenance`: Taint chain from originating zone through all processing steps
- Taints: Secret, Elevation, DeclassificationLog, Export
- All operations carry taint chain for audit

### Error Taxonomy
FCP-1xxx (Protocol), FCP-2xxx (Auth), FCP-3xxx (Capability), FCP-4xxx (Zone), FCP-5xxx (Connector), FCP-6xxx (Resource), FCP-7xxx (External), FCP-9xxx (Internal)

---

## 10. Constraints for FrankenTerm Integration

### Safety Requirements
1. `#![forbid(unsafe_code)]` in all core crates (fcp-sandbox exception for OS syscalls)
2. Symbol-as-atomic-unit: All persistent data must be RaptorQ-codable
3. Zone isolation: Never leak zone-bound secrets across zones
4. Audit everything: Secrets, elevations, cross-zone ops emit AuditEvent
5. Content addressing: Use ObjectId::new(content, zone, schema) for security objects
6. Zeroize: All secret bytes wiped immediately after use
7. Threshold secrets: No single point holds complete secret
8. Holder proof: Operations with holder_node binding require request signature

### What FrankenTerm Should Adopt
1. **Manifest-based connector governance** — Declare capabilities, rate limits, sandbox profile
2. **Capability token model** — Signed authorization proofs with constraints
3. **Audit chain** — Hash-linked events with sequence numbers (already partially implemented in policy_audit_chain.rs)
4. **Canary rollout** — Already implemented in canary_rollout_controller.rs
5. **Health monitoring pattern** — HealthSnapshot/SelfCheckReport/Liveness
6. **Egress guard** — Deny-by-default network policy per operation
7. **Provenance tracking** — Taint chain for data classification

### What to Adapt (Not Direct Port)
1. **Transport layer** — RaptorQ/Tailscale are mesh-specific; FrankenTerm uses different network model
2. **WASI runtime** — wasmtime integration is optional; FrankenTerm may prefer process-level isolation
3. **Zone key management** — Simplify for single-node operation; scale up later for distributed mode
4. **Gossip protocol** — Only needed for multi-node mesh; skip for local-first mode

### Compatibility Notes
- Rust 2024 edition: `std::env::set_var` unsafe (use Command::env())
- Instant serialization: Don't derive serde on structs with Instant
- Manifest interface_hash: Recompute when operation/capability signatures change

## 11. Adopt / Adapt / Defer Handoff Matrix

This is the implementation-facing mapping from FCP source concepts to current
FrankenTerm seams and the downstream tracks that should absorb each concept.

| Capability Family | Decision | FCP Source Anchors | FrankenTerm Anchor(s) | Primary Downstream Tracks |
|---|---|---|---|---|
| Manifest schema, interface hash, capability declarations | Adopt | `crates/fcp-manifest/src/lib.rs`, `FCP_CDDL_V2.cddl` | `connector_registry.rs`, `connector_bundles.rs`, `connector_sdk.rs` | `ft-3681t.5.1`, `ft-3681t.5.12`, `ft-3681t.6.1` |
| Registry endorsement and supply-chain validation | Adopt | `crates/fcp-registry/src/lib.rs` | `connector_registry.rs`, bundle registry, policy trust gates | `ft-3681t.5.9`, `ft-3681t.5.12`, `ft-3681t.6.4` |
| Sandbox zones and deny-by-default egress | Adapt | `crates/fcp-sandbox/src/lib.rs`, `FCP_Specification_V3.md` | `connector_host_runtime.rs`, `policy.rs`, `connector_governor.rs` | `ft-3681t.5.3`, `ft-3681t.5.4`, `ft-3681t.6.1` |
| Capability tokens, approvals, holder-proof style anti-replay | Adapt then adopt | `FCP_CDDL_V2.cddl`, `FCP_Specification_V3.md` | `connector_sdk.rs`, `policy.rs`, `approval.rs`, `connector_credential_broker.rs` | `ft-3681t.5.4`, `ft-3681t.5.6`, `ft-3681t.6.*` |
| Provenance, classification, audit chain, decision receipts | Adopt | `crates/fcp-audit/src/lib.rs`, `crates/fcp-core/src/audit.rs`, `FCP_CDDL_V2.cddl` | `policy_audit_chain.rs`, `connector_data_classification.rs`, `policy.rs` | `ft-3681t.5.14`, `ft-3681t.6.4`, `ft-3681t.7.1` |
| Mesh federation, placement, RaptorQ-backed transport | Defer for local-first, adapt for distributed mode | `crates/fcp-mesh/src/lib.rs`, `FCP_Specification_V3.md` | `connector_mesh.rs`, `distributed.rs`, headless mux server work | `ft-3681t.2.6`, distributed connector follow-ons in `ft-3681t.5.*` |

## 12. Failure-Mode Obligations To Preserve

The source material makes several failure behaviors normative. FrankenTerm
should preserve these even when the transport/runtime implementation differs.

| Concern | Required FrankenTerm Behavior | Current Anchor |
|---|---|---|
| Manifest mismatch or signature failure | Fail package admission before runtime startup and surface a typed trust error | `connector_registry.rs`, `connector_bundles.rs` |
| Sandbox ambiguity or egress-policy mismatch | Fail closed, emit auditable reason code, do not attempt best-effort execution | `connector_host_runtime.rs`, `policy.rs` |
| Credential revocation or approval expiry | Invalidate brokered access immediately and reject subsequent operations deterministically | `connector_credential_broker.rs`, approval/revocation state in `policy.rs` |
| Canary health regression | Hold or roll back promotion, preserve evidence bundle, do not silently continue in production | `connector_lifecycle.rs`, `canary_rollout_controller.rs`, `connector_reliability.rs` |
| Audit-chain fork, gap, or sequence violation | Escalate as a security event, quarantine the connector/runtime slice, require operator review | `policy_audit_chain.rs`, compliance/audit surfaces in `policy.rs` |

---

## 13. Crate Map

| Crate | Purpose | FrankenTerm Relevance |
|-------|---------|----------------------|
| fcp-core | Core types, traits, errors | **HIGH** — Study trait model |
| fcp-host | Host orchestrator | **HIGH** — Connector supervision pattern |
| fcp-sandbox | OS sandbox + egress guard | **MEDIUM** — Adapt for ft connectors |
| fcp-protocol | Wire protocol (FCPS/FCPC) | **LOW** — Different transport |
| fcp-mesh | Mesh routing, gossip | **LOW** — Not needed for local mode |
| fcp-registry | Supply-chain verification | **MEDIUM** — Adopt for connector trust |
| fcp-manifest | Manifest parsing | **HIGH** — Adopt manifest model |
| fcp-sdk | Connector SDK | **MEDIUM** — Re-exports + utilities |
| fcp-crypto | Ed25519, X25519, HPKE | **LOW** — Already have crypto deps |
| fcp-raptorq | Fountain codes | **LOW** — Mesh-specific |
| fcp-store | Object persistence | **MEDIUM** — Study content addressing |
| fcp-audit | Audit chain primitives | **HIGH** — Enhance existing audit chain |

---

*END OF INVENTORY*
