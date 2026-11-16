# Non-Canonical Analyses

This directory holds 24 `COMPREHENSIVE_ANALYSIS_OF_<subject>.md` files filed
in a single February 2026 sweep. They are **aspirational** pre-planning /
scoping documents, NOT authoritative descriptions of what FrankenTerm
actually ships today.

Agents and humans reading these should treat them the way you'd treat an
old design note: useful for motivation and vocabulary, dangerous if you
assume every capability they describe is live. When this index conflicts
with `AGENTS.md`, `README.md`, or code in `crates/`, the code and
`AGENTS.md` win.

## Why this file exists

Filed for `ft-e9ue4` ([MEDIUM] audit bead). The original audit flagged
these analyses as lacking any freshness / status marker — a reader
landing on one of them had no way to know whether it described the
current state of the subject or a speculative roadmap. Rather than
prepend a 3-line frontmatter block to each of 24 files, we centralize
the disclosure here and add a single header pointer to each analysis.

## Status legend

| Marker | Meaning |
|--------|---------|
| 🟢 shipped | The subject exists in-tree and is actively used. The analysis may still be out of date on specifics. |
| ⚪ tooling | Used in the development environment; this does not establish an in-product integration. |
| 🟡 partial | Some of what the analysis describes is live; significant parts remain aspirational. |
| 🔵 external | The subject is a separate repo this project depends on. Analysis is a snapshot; the external repo is authoritative. |
| 🔴 aspirational | Nothing (or almost nothing) of the subject has shipped in FrankenTerm. |
| ⏸ skipped | The integration plan deliberately rejects or defers this product integration. |

Status reviewed against source on 2026-09-04. A dependency, exported module,
installed CLI, or passing isolated test does not prove a complete integration.
Partial entries identify the reachable product path and its principal gap;
runtime/release certification requires separate retained evidence.

## Index

| Status | Subject | Original analysis | Filed | Canonical source |
|--------|---------|-------------------|-------|------------------|
| ⚪ tooling | beads_rust | [COMPREHENSIVE_ANALYSIS_OF_beads_rust.md](./COMPREHENSIVE_ANALYSIS_OF_beads_rust.md) | 2026-02-22 | `br` developer CLI; mission/work-queue integration has separate acceptance |
| ⚪ tooling | beads_viewer_rust | [COMPREHENSIVE_ANALYSIS_OF_beads_viewer_rust.md](./COMPREHENSIVE_ANALYSIS_OF_beads_viewer_rust.md) | 2026-02-22 | `bv` developer CLI; `graph_scoring` has no production consumer |
| 🟢 shipped | coding_agent_session_search | [COMPREHENSIVE_ANALYSIS_OF_coding_agent_session_search.md](./COMPREHENSIVE_ANALYSIS_OF_coding_agent_session_search.md) | 2026-02-22 | `ft robot cass` family + `cass` daemon |
| 🟡 partial | cross_agent_session_resumer | [COMPREHENSIVE_ANALYSIS_OF_cross_agent_session_resumer.md](./COMPREHENSIVE_ANALYSIS_OF_cross_agent_session_resumer.md) | 2026-02-22 | CLI provider discovery/resume is reachable; recorder-to-conversation conversion is absent |
| ⚪ tooling | destructive_command_guard | [COMPREHENSIVE_ANALYSIS_OF_destructive_command_guard.md](./COMPREHENSIVE_ANALYSIS_OF_destructive_command_guard.md) | 2026-02-22 | Developer hooks/skill are distinct from `ft` policy enforcement |
| 🟡 partial | fastapi_rust | [COMPREHENSIVE_ANALYSIS_OF_fastapi_rust.md](./COMPREHENSIVE_ANALYSIS_OF_fastapi_rust.md) | 2026-02-22 | Localhost read/SSE service; WebSocket, authentication, OpenAPI and richer HTTP mutations are unimplemented |
| 🟡 partial | fastmcp_ruse | [COMPREHENSIVE_ANALYSIS_OF_fastmcp_ruse.md](./COMPREHENSIVE_ANALYSIS_OF_fastmcp_ruse.md) | 2026-02-22 | Real server and feature-gated subprocess proxy; caller deadlines/transport cancellation remain tracked |
| 🟡 partial | frankensearch | [COMPREHENSIVE_ANALYSIS_OF_frankensearch.md](./COMPREHENSIVE_ANALYSIS_OF_frankensearch.md) | 2026-02-22 | Lexical/hash semantic/hybrid baseline; learned pipeline/reranker integration remains open |
| 🟢 shipped | frankentui | [COMPREHENSIVE_ANALYSIS_OF_frankentui.md](./COMPREHENSIVE_ANALYSIS_OF_frankentui.md) | 2026-02-21 | `ftui-*` workspace deps; `ftui` feature |
| 🟡 partial | mcp_agent_mail_rust | [COMPREHENSIVE_ANALYSIS_OF_mcp_agent_mail_rust.md](./COMPREHENSIVE_ANALYSIS_OF_mcp_agent_mail_rust.md) | 2026-02-22 | External coordination tool; product bridge/lifecycle and durable outbox pending ft-7c21y/ft-hb7nt |
| ⚪ tooling | process_triage | [COMPREHENSIVE_ANALYSIS_OF_process_triage.md](./COMPREHENSIVE_ANALYSIS_OF_process_triage.md) | 2026-02-21 | `pt` developer tool; product has a local heuristic, not PtCli/PtEmbedded lifecycle integration |
| ⚪ tooling | remote_compilation_helper | [COMPREHENSIVE_ANALYSIS_OF_remote_compilation_helper.md](./COMPREHENSIVE_ANALYSIS_OF_remote_compilation_helper.md) | 2026-02-22 | Required development Cargo offload; see AGENTS.md |
| ⚪ tooling | storage_ballast_helper | [COMPREHENSIVE_ANALYSIS_OF_storage_ballast_helper.md](./COMPREHENSIVE_ANALYSIS_OF_storage_ballast_helper.md) | 2026-02-21 | `sbh` developer tool; product disk monitoring is a separate surface |
| ⚪ tooling | ultimate_bug_scanner | [COMPREHENSIVE_ANALYSIS_OF_ultimate_bug_scanner.md](./COMPREHENSIVE_ANALYSIS_OF_ultimate_bug_scanner.md) | 2026-02-22 | `ubs` developer tool; `CodeScanner` wrapper has no product caller |
| 🟡 partial | franken_redis | [COMPREHENSIVE_ANALYSIS_OF_franken_redis.md](./COMPREHENSIVE_ANALYSIS_OF_franken_redis.md) | 2026-02-21 | Actual fr-* dependencies and SessionStore substrate; no live SessionStore consumer found; master plan defers product adoption |
| 🔵 external | franken_agent_detection | [COMPREHENSIVE_ANALYSIS_OF_franken_agent_detection.md](./COMPREHENSIVE_ANALYSIS_OF_franken_agent_detection.md) | 2026-02-22 | `franken-agent-detection` workspace dep |
| 🔵 external | franken_mermaid | [COMPREHENSIVE_ANALYSIS_OF_franken_mermaid.md](./COMPREHENSIVE_ANALYSIS_OF_franken_mermaid.md) | 2026-02-21 | External rendering crate; not integrated in-tree |
| 🔵 external | frankensqlite | [COMPREHENSIVE_ANALYSIS_OF_frankensqlite.md](./COMPREHENSIVE_ANALYSIS_OF_frankensqlite.md) | 2026-02-22 | External crate; recorder selector rejects `frankensqlite` until the backend implementation ships (see ft-0v48y and ft-kcdqp) |
| ⏸ skipped | agentic_coding_flywheel_setup | [COMPREHENSIVE_ANALYSIS_OF_agentic_coding_flywheel_setup.md](./COMPREHENSIVE_ANALYSIS_OF_agentic_coding_flywheel_setup.md) | 2026-02-22 | Development tooling; product integration explicitly rejected by its plan |
| ⏸ skipped | automated_plan_reviser_pro | [COMPREHENSIVE_ANALYSIS_OF_automated_plan_reviser_pro.md](./COMPREHENSIVE_ANALYSIS_OF_automated_plan_reviser_pro.md) | 2026-02-22 | Product integration explicitly rejected by its plan |
| 🟡 partial | coding_agent_usage_tracker | [COMPREHENSIVE_ANALYSIS_OF_coding_agent_usage_tracker.md](./COMPREHENSIVE_ANALYSIS_OF_coding_agent_usage_tracker.md) | 2026-02-22 | CautClient accounts/refresh has CLI callers; full per-pane cost integration is not certified |
| 🟡 partial | rano | [COMPREHENSIVE_ANALYSIS_OF_rano.md](./COMPREHENSIVE_ANALYSIS_OF_rano.md) | 2026-02-22 | NetworkObserver wrapper contract correction pending ft-6jj9m; no certified live pressure path |
| ⏸ skipped | rust_proxy | [COMPREHENSIVE_ANALYSIS_OF_rust_proxy.md](./COMPREHENSIVE_ANALYSIS_OF_rust_proxy.md) | 2026-02-22 | Product integration explicitly rejected by its plan |
| 🟡 partial | vibe_cockpit | [COMPREHENSIVE_ANALYSIS_OF_vibe_cockpit.md](./COMPREHENSIVE_ANALYSIS_OF_vibe_cockpit.md) | 2026-02-22 | `vc_export` library exists; CLI export/VC collector roundtrip has no product consumer |

## Updating this index

When a `COMPREHENSIVE_ANALYSIS_OF_<subject>.md` transitions from
aspirational → shipped / partial, update the status column and add the
canonical source pointer. When the underlying analysis is refreshed (or
rewritten as a canonical doc), move it out of this directory and remove
the row.
