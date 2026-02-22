# Comprehensive Analysis of agentic_coding_flywheel_setup

> Analysis document for FrankenTerm bead `ft-2vuw7.30.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**agentic_coding_flywheel_setup** (ACFS) is a TypeScript + Bash monorepo for bootstrapping Ubuntu VPS environments with ~60 AI coding tools. It uses a manifest-driven architecture (YAML → generated bash installers) with Zod schema validation, a Next.js 16 wizard UI, and resumable installation with checkpoints. Not a Rust project.

**Integration Value**: Low — VPS provisioning tool; orthogonal to FrankenTerm's terminal multiplexer role.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Languages** | TypeScript, Bash |
| **Crate Count** | N/A (not Rust) |
| **Version** | 0.6.0 |
| **License** | MIT |
| **Platform** | Ubuntu 25.10 (target VPS) |
| **Runtime** | Bun (Node.js) + Bash |

### Workspace Structure

```
acfs/
├── apps/web/              # Next.js 16 wizard UI
├── packages/manifest/     # TypeScript manifest parser + generator
├── packages/onboard/      # Interactive onboarding lessons
├── acfs.manifest.yaml     # Single source of truth (60 modules)
├── install.sh             # Bootstrap script (5.6K LOC)
├── scripts/lib/           # Bash libraries (~40K LOC)
├── scripts/generated/     # Auto-generated category installers
├── checksums.yaml         # SHA256 verification database
```

---

## Core Architecture

### Manifest-Driven Design

`acfs.manifest.yaml` defines all ~60 tools. From this single file:
1. Generated bash installers (per-category)
2. Doctor health checks (post-install verification)
3. Web content (tools pages, wizard)
4. Type-safe validation (Zod schemas)

### Installation Pipeline

```
curl | bash → install.sh → parse args → resolve DAG
  → phase execution (1-10) → per-module install
  → state checkpoints → doctor health checks
```

### Key Features

- **Resume/checkpoint**: `~/.acfs/state.json` tracks per-module progress
- **Idempotent**: Safe to re-run; already-installed tools skip
- **Security**: HTTPS enforcement, checksum verification, verified_installer allowlist
- **40+ error patterns**: Mapping shell errors to remediation steps

---

## FrankenTerm Integration Assessment

### Relevance

| Factor | Assessment |
|--------|-----------|
| **Language** | TypeScript + Bash (not Rust) |
| **Use Case** | VPS provisioning; one-time setup |
| **Platform** | Ubuntu-only target |
| **Overlap** | Installs tools FrankenTerm agents use (claude, codex, tmux, etc.) |

### Potential (Limited) Integration

1. **Environment detection**: FrankenTerm could detect ACFS installation via `~/.acfs/`
2. **Tool availability**: Check ACFS state for installed tool versions
3. **Manifest as tool registry**: Read `acfs.manifest.yaml` for available tools

### Recommendation

**No deep integration needed** — ACFS is a one-time provisioning tool. FrankenTerm benefits from ACFS-provisioned environments but doesn't need to interact with ACFS at runtime.

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Manifest-driven VPS bootstrap (TypeScript + Bash) |
| **Key Innovation** | Single YAML manifest → generated installers + validation |
| **FrankenTerm Status** | No integration |
| **Integration Priority** | Low |
| **New Modules Needed** | 0 |
| **Recommendation** | No integration; ACFS provisions, FrankenTerm operates |

---

*Analysis complete.*
