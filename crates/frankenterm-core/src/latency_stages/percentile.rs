//! br-ft-l8s7v slice 1: extracted from `latency_stages.rs`.
//!
//! Percentile-level enum for latency budgets. Self-contained
//! primitive — no `latency_stages` dependencies — so it's the
//! cleanest first slice of the larger `latency_stages.rs`
//! decomposition (29,611 lines, 463 top-level items).
//!
//! Re-exported from the parent module via `pub use` so existing
//! `latency_stages::Percentile` paths continue to resolve.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Percentile levels for latency budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Percentile {
    P50,
    P95,
    P99,
    P999,
}

impl Percentile {
    /// All percentile levels in ascending order.
    pub const ALL: &[Self] = &[Self::P50, Self::P95, Self::P99, Self::P999];

    /// The numeric percentile value (e.g., 0.999 for P999).
    pub fn value(self) -> f64 {
        match self {
            Self::P50 => 0.50,
            Self::P95 => 0.95,
            Self::P99 => 0.99,
            Self::P999 => 0.999,
        }
    }
}

impl fmt::Display for Percentile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::P50 => f.write_str("p50"),
            Self::P95 => f.write_str("p95"),
            Self::P99 => f.write_str("p99"),
            Self::P999 => f.write_str("p999"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_contains_every_variant_in_ascending_order() {
        assert_eq!(
            Percentile::ALL,
            &[
                Percentile::P50,
                Percentile::P95,
                Percentile::P99,
                Percentile::P999
            ]
        );
    }

    #[test]
    fn value_returns_documented_floats() {
        assert!((Percentile::P50.value() - 0.50).abs() < f64::EPSILON);
        assert!((Percentile::P95.value() - 0.95).abs() < f64::EPSILON);
        assert!((Percentile::P99.value() - 0.99).abs() < f64::EPSILON);
        assert!((Percentile::P999.value() - 0.999).abs() < f64::EPSILON);
    }

    #[test]
    fn display_emits_lowercase_p_prefix() {
        assert_eq!(format!("{}", Percentile::P50), "p50");
        assert_eq!(format!("{}", Percentile::P95), "p95");
        assert_eq!(format!("{}", Percentile::P99), "p99");
        assert_eq!(format!("{}", Percentile::P999), "p999");
    }

    #[test]
    fn ordering_is_ascending_by_percentile() {
        let mut sorted = vec![
            Percentile::P999,
            Percentile::P50,
            Percentile::P99,
            Percentile::P95,
        ];
        sorted.sort();
        assert_eq!(sorted, Percentile::ALL.to_vec());
    }

    #[test]
    fn serde_roundtrip() {
        for p in Percentile::ALL {
            let json = serde_json::to_string(p).expect("serialize");
            let back: Percentile = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*p, back);
        }
    }
}
