# Comprehensive Analysis of rust_proxy

> Analysis document for FrankenTerm bead `ft-2vuw7.22.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**rust_proxy** is an 18K LOC single-crate Rust binary implementing a transparent HTTP CONNECT proxy with iptables/ipset integration, 4 load-balancing strategies, health checking, and Prometheus metrics. It is Linux-only and has no current FrankenTerm integration.

**Integration Value**: Low — Linux-only proxy with limited relevance to FrankenTerm's terminal multiplexer use case.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~18,000 |
| **Crate Count** | 1 (single binary) |
| **Rust Edition** | 2021 |
| **MSRV** | 1.70+ |
| **License** | MIT |
| **Platform** | Linux-only (iptables, ipset) |

### Key Modules

- **proxy.rs**: HTTP CONNECT tunnel implementation
- **lb.rs**: Load balancing (round-robin, weighted, least-connections, random)
- **health.rs**: Upstream health checking with circuit breaker
- **metrics.rs**: Prometheus /metrics endpoint
- **config.rs**: TOML configuration with hot-reload
- **iptables.rs**: Linux iptables/ipset integration for transparent proxying

---

## Core Architecture

### Proxy Pipeline

```
Client → [Accept] → [Parse CONNECT] → [LB Strategy] → [Upstream Select]
     → [Health Check] → [TCP Tunnel] → [Bidirectional Copy] → [Metrics]
```

### Load Balancing Strategies

1. **Round Robin**: Sequential rotation
2. **Weighted Round Robin**: Weight-based distribution
3. **Least Connections**: Route to least-loaded upstream
4. **Random**: Uniform random selection

### Health Checking

- Active TCP probes at configurable intervals
- Circuit breaker pattern (half-open → open → closed)
- Prometheus health status metrics per upstream

---

## FrankenTerm Integration Assessment

### Relevance

| Factor | Assessment |
|--------|-----------|
| **Platform** | Linux-only; FrankenTerm is cross-platform (macOS primary) |
| **Use Case** | Network proxy; FrankenTerm is terminal multiplexer |
| **Shared Types** | None |
| **Runtime** | tokio-based; compatible but no shared abstractions |

### Potential (Limited) Integration

1. **Proxy health in dashboard**: If FrankenTerm agents route through rust_proxy, display upstream health
2. **Metrics bridge**: Import Prometheus metrics for network quality monitoring
3. **Algorithm extraction**: Circuit breaker pattern already implemented in FrankenTerm

### Recommendation

**Minimal integration** — rust_proxy is a standalone network tool. FrankenTerm already has its own circuit breaker implementation. The only viable integration would be a thin status reader if agents happen to route through the proxy.

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Linux-only dependency | High | High | Cannot use on macOS development |
| Overlapping circuit breaker | Medium | Low | FrankenTerm already has one |
| Maintenance burden | Medium | Low | Skip integration entirely |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Single-crate transparent HTTP proxy |
| **Key Innovation** | iptables/ipset transparent proxying |
| **FrankenTerm Status** | No integration |
| **Integration Priority** | Low |
| **New Modules Needed** | 0-1 (optional status reader) |
| **Recommendation** | Skip or minimal status bridge only |

---

*Analysis complete.*
