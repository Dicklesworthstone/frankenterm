# Comprehensive Analysis of FrankenSQLite

> Bead: ft-2vuw7.1.1 / ft-2vuw7.1.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

FrankenSQLite (`/dp/frankensqlite`, binary: `fsqlite`) is a production-grade, clean-room reimplementation of SQLite in ~826K LOC of pure Rust across 26 crates. It is 100% file-format compatible with C SQLite 3.52.0, enforces `#![forbid(unsafe_code)]` workspace-wide, and adds page-level MVCC with SSI (Serializable Snapshot Isolation), WAL durability via RaptorQ fountain codes, and ChaCha20-Poly1305 encryption.

**Key characteristics:**
- 26-crate workspace: types, error, VFS, pager, WAL, B-tree, MVCC, parser, AST, planner, VDBE, functions, 7 extensions, core, facade, CLI, C-API, observability, test harness, E2E
- `#![forbid(unsafe_code)]` workspace-wide (Rust 2024, MSRV 1.85)
- 100% SQLite 3.x file format compatibility (database header, B-tree pages, WAL frames, record encoding)
- MVCC: `BEGIN CONCURRENT` with page-level locks, SSI conflict detection, lock-free commit sequence
- WAL with RaptorQ repair symbols for self-healing torn writes (asupersync 0.2.5)
- ChaCha20-Poly1305 encryption with Argon2id key derivation
- 3 VFS backends: Memory, IoUring (Linux), Unix (POSIX)
- FTS5, JSON1, R-tree, ICU, Session extensions (feature-gated)

**Integration relevance to FrankenTerm:** Very High. FrankenSQLite can replace rusqlite as FrankenTerm's storage layer, providing concurrent writer support for multi-agent session logging, MVCC for read-consistent snapshots during session recovery, and RaptorQ durability for critical terminal history.

---

## 2. Repository Topology

### 2.1 Workspace Structure (26 Crates)

```
/dp/frankensqlite/   (v0.1.0, edition 2024, MSRV 1.85, #![forbid(unsafe_code)])
├── crates/
│   ├── fsqlite-types/          — Value types, page abstractions
│   ├── fsqlite-error/          — FrankenError (247 variants), error codes
│   ├── fsqlite-vfs/            — Virtual filesystem (Memory, IoUring, Unix)
│   ├── fsqlite-pager/          — Page cache (ArcCache: S3-FIFO + LRU)
│   ├── fsqlite-wal/            — Write-Ahead Log with RaptorQ repair symbols
│   ├── fsqlite-btree/          — B-tree cursors, page traversal
│   ├── fsqlite-mvcc/           — Page-level MVCC, SSI conflict detection
│   ├── fsqlite-parser/         — Hand-written recursive descent SQL parser
│   ├── fsqlite-ast/            — SQL AST types
│   ├── fsqlite-planner/        — Query planner
│   ├── fsqlite-vdbe/           — Bytecode VM (Virtual Database Engine)
│   ├── fsqlite-func/           — Built-in scalar/aggregate/window functions
│   ├── fsqlite-ext-json/       — JSON1 extension
│   ├── fsqlite-ext-fts3/       — Full-text search v3
│   ├── fsqlite-ext-fts5/       — Full-text search v5
│   ├── fsqlite-ext-rtree/      — R-tree spatial indexing
│   ├── fsqlite-ext-icu/        — Unicode collation
│   ├── fsqlite-ext-session/    — Change tracking
│   ├── fsqlite-ext-misc/       — Miscellaneous extensions
│   ├── fsqlite-core/           — Connection API (22.8K LOC connection.rs)
│   ├── fsqlite/                — Public facade crate
│   ├── fsqlite-cli/            — Interactive SQL shell
│   ├── fsqlite-c-api/          — C FFI bindings
│   ├── fsqlite-observability/  — Tracing, SSI decision cards
│   ├── fsqlite-harness/        — Test infrastructure
│   └── fsqlite-e2e/            — End-to-end test suites
```

Total Rust LOC: ~826K

### 2.2 Key Dependencies

| Dep | Version | Purpose |
|-----|---------|---------|
| asupersync | 0.2.5 | RaptorQ rateless fountain codes (WAL repair) |
| parking_lot | 0.12 | Efficient sync primitives |
| crossbeam-epoch | 0.9 | Lock-free memory reclamation |
| crossbeam-deque | 0.8 | Concurrent work-stealing queues |
| xxhash-rust | 0.8 (xxh3) | Fast hashing |
| blake3 | 1.5 | Content-addressed hashing |
| sha2 | 0.10 | SHA-256 for integrity |
| chacha20poly1305 | 0.10 | AEAD encryption |
| argon2 | 0.5 | Key derivation |
| crc32c | 0.6 | Page checksums |
| memchr | 2.7 | Fast substring search |
| smallvec | 1.13 | Stack-allocated vectors |
| bitflags | 2.9 | Bit manipulation |
| tracing | 0.1 | Structured logging |
| nix | 0.29 (fs) | POSIX syscalls |
| hashbrown | 0.14 | No-std HashMap |
| rusqlite | 0.32 (bundled) | Compatibility test oracle only |

### 2.3 Feature Flags

```toml
[features]
default = ["json", "fts5", "rtree"]
json    = ["dep:fsqlite-ext-json"]       # JSON1 extension
fts3    = ["dep:fsqlite-ext-fts3"]       # Full-text search v3
fts5    = ["dep:fsqlite-ext-fts5"]       # Full-text search v5
rtree   = ["dep:fsqlite-ext-rtree"]      # R-tree spatial indexing
session = ["dep:fsqlite-ext-session"]    # Change tracking
icu     = ["dep:fsqlite-ext-icu"]        # Unicode collation
misc    = ["dep:fsqlite-ext-misc"]       # Miscellaneous
raptorq = []                              # RaptorQ codec control
mvcc    = []                              # MVCC mode toggle
```

---

## 3. Core Architecture

### 3.1 Execution Pipeline

```
SQL → Parser (recursive descent) → AST → Planner → Codegen → VDBE bytecode
    → Engine.execute(program, MemDatabase)
    → DDL: schema/trigger/view updates
    → DML: B-tree mutations + trigger firing
    → SELECT: row collection + DISTINCT/LIMIT
    → Results: Vec<Row>
```

### 3.2 Storage Layers

```
Application
    ↓
Connection (22.8K LOC) — transaction lifecycle, prepared statements, triggers
    ↓
MVCC — page-level versioning, SSI conflict detection, lock-free commit clock
    ↓
B-tree — cursor-based traversal, page splits/merges
    ↓
Pager — ArcCache (S3-FIFO + LRU), page allocation, dirty tracking
    ↓
WAL — frame-based log, RaptorQ repair symbols, checkpoint modes
    ↓
VFS — Memory | IoUring (Linux) | Unix (POSIX)
```

### 3.3 MVCC Concurrency Model

```
BEGIN CONCURRENT
  → Assign TxnToken + snapshot_seq from AtomicU64
  → Page-level locks via InProcessPageLockTable (sharded, cache-line padded)
  → SSI validation at commit (detect read-write conflicts)
  → Lock-free commit sequence advancement

Conflict Resolution:
  → WriteConflict: same page modified by concurrent writers
  → SerializationFailure: SSI cycle detected
  → BusySnapshot: snapshot too old (WAL checkpoint advanced past it)
  → All classified as is_transient() → auto-retry candidates
```

### 3.4 WAL Durability

```
WAL frame write:
  → Append data + checksum
  → Generate RaptorQ source symbols
  → Append repair symbols (fountain codes)
  → On read: detect torn write → recover via repair symbols
  → Metrics: GLOBAL_WAL_FEC_REPAIR_METRICS tracks repair events

Checkpoint modes: Passive, Restart, Full
```

### 3.5 Public API

```rust
// Connection lifecycle
Connection::open(path) → Result<Connection>
conn.close() → Result<()>

// Query execution
conn.prepare(sql) → Result<PreparedStatement>
conn.query(sql) → Result<Vec<Row>>
conn.query_with_params(sql, params) → Result<Vec<Row>>
conn.execute(sql) → Result<usize>  // affected rows

// UDF registration
conn.register_scalar_function(f)
conn.register_aggregate_function(f)
conn.register_window_function(f)

// MVCC observability
conn.concurrent_writer_count() → usize
conn.ssi_decisions_snapshot() → Vec<SsiDecisionCard>
conn.raptorq_repair_evidence_snapshot() → Vec<WalFecRepairEvidenceCard>

// Tracing (sqlite3_trace_v2 compatible)
conn.trace_v2(mask, callback)
```

### 3.6 Error Model

`FrankenError` enum with 247 variants covering:
- Database state (Locked, Corrupt, Full, SchemaChanged)
- SQL errors (SyntaxError, NoSuchTable/Column/Index)
- Constraints (Unique, NotNull, Check, ForeignKey)
- MVCC (WriteConflict, SerializationFailure, SnapshotTooOld)
- Busy (Busy, BusyRecovery, BusySnapshot)

Helper methods: `is_transient()`, `is_user_recoverable()`, `suggestion()`, `error_code()` (C SQLite compatible)

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Replacement Path

FrankenTerm currently uses `rusqlite 0.32` (C SQLite). FrankenSQLite offers a drop-in replacement:

| Aspect | rusqlite (Current) | FrankenSQLite (Target) |
|--------|-------------------|----------------------|
| Safety | C code + FFI | `#![forbid(unsafe_code)]` |
| Concurrency | Single writer | MVCC concurrent writers |
| Durability | Standard WAL | WAL + RaptorQ repair |
| Encryption | Compile-time SQLCipher | Built-in ChaCha20 |
| File format | SQLite 3.x | SQLite 3.x (compatible) |

### 4.2 Shared Components

| Component | FrankenTerm Use Case | Effort |
|-----------|---------------------|--------|
| **Connection API** | Replace rusqlite for session/audit storage | Medium |
| **MVCC** | Concurrent agent writes to shared DB | Low (API-level) |
| **WAL + RaptorQ** | Self-healing storage for flight recorder | Low |
| **FTS5** | Full-text search in session history | Low |
| **Observability** | SSI decision cards in health dashboard | Low |
| **Error model** | Structured error classification for retry logic | Low |

### 4.3 Key Benefits for FrankenTerm

1. **Multi-agent concurrent writes**: `BEGIN CONCURRENT` allows multiple agents to write terminal session logs without lock contention
2. **Read-consistent snapshots**: MVCC provides point-in-time consistency during session recovery
3. **Self-healing storage**: RaptorQ repair symbols recover from torn writes (power loss, crash)
4. **No C dependency**: Pure Rust, fully auditable, no FFI attack surface
5. **Same file format**: Migration is transparent — existing `.sqlite` files work unchanged

### 4.4 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Large codebase (826K LOC) | Medium | Import only `fsqlite` facade crate |
| API differences from rusqlite | Medium | Similar patterns; migration guide in crate docs |
| Maturity vs. C SQLite | Medium | Comprehensive test suite; rusqlite as test oracle |
| asupersync dependency | Low | Shared with FrankenTerm already |
| Connection not Send/Sync | Low | FrankenTerm already uses per-task connections |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Pure-Rust SQLite reimplementation with MVCC + RaptorQ durability |
| **Size** | ~826K LOC, 26 crates, Rust 2024 |
| **Architecture** | Parser → AST → Planner → VDBE → B-tree → Pager → WAL → VFS |
| **Safety** | `#![forbid(unsafe_code)]`, ChaCha20 encryption, Argon2 KDF |
| **Performance** | Lock-free commit clock, S3-FIFO+LRU cache, io_uring (Linux) |
| **Integration Value** | Very High — concurrent writers, self-healing WAL, pure Rust |
| **Top Extraction** | MVCC concurrent writes, RaptorQ repair, FTS5, observability |
| **Risk** | Medium — large codebase, but clean facade API |
