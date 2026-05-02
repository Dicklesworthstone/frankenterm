//! `[render.triple_buffer]` TOML config-reload substrate
//! (br-ft-zkahy sub-task 7).
//!
//! The fleet-health substrate
//! ([`crate::triple_buffer_fleet_health`]) ships
//! [`TripleBufferReconfigureAuditEvent`] +
//! [`ReconfigureSource`]; this module ships the **TOML
//! deserialization + invariant validation + event-builder**
//! pipeline the GUI's frankenterm.toml reload path drives.
//!
//! Sub-task 7 of ft-zkahy:
//!
//! > "frankenterm.toml [render.triple_buffer] reload: emit
//! > substrate's TripleBufferReconfigureAuditEvent into
//! > audit log; call wtb.reconfigure_watchdog per pane."
//!
//! [`TripleBufferReconfigureAuditEvent`]: crate::triple_buffer_fleet_health::TripleBufferReconfigureAuditEvent
//! [`ReconfigureSource`]: crate::triple_buffer_fleet_health::ReconfigureSource
//!
//! ## TOML schema
//!
//! ```toml
//! [render.triple_buffer]
//! warn_after_ms = 1000
//! force_recycle_after_ms = 5000
//! ```
//!
//! Both fields are required; missing fields fail
//! [`parse_render_triple_buffer_section`] so operators can't
//! accidentally ship a half-configured section.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::triple_buffer_fleet_health::{
    PaneId, ReconfigureSource, TripleBufferReconfigureAuditEvent,
};
use crate::triple_buffer_watchdog::WatchdogConfig;

/// Maximum permitted `force_recycle_after_ms`. Operators
/// shouldn't be able to disable the stuck-reader protection
/// by setting a multi-day timeout; cap at one minute.
pub const MAX_FORCE_RECYCLE_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripleBufferConfigSection {
    pub warn_after_ms: u64,
    pub force_recycle_after_ms: u64,
}

impl TripleBufferConfigSection {
    /// Convert to a [`WatchdogConfig`]. Validation should run
    /// first via [`Self::validate`]; this conversion only does
    /// the unit translation.
    #[must_use]
    pub fn to_watchdog_config(self) -> WatchdogConfig {
        WatchdogConfig::default()
            .with_warn_after(Duration::from_millis(self.warn_after_ms))
            .with_force_recycle_after(Duration::from_millis(
                self.force_recycle_after_ms,
            ))
    }

    /// Snapshot a [`WatchdogConfig`] back into a config
    /// section (for prev-state capture in
    /// [`build_reconfigure_event`]).
    #[must_use]
    pub fn from_watchdog_config(cfg: &WatchdogConfig) -> Self {
        Self {
            warn_after_ms: cfg.warn_after().as_millis() as u64,
            force_recycle_after_ms: cfg.force_recycle_after().as_millis() as u64,
        }
    }

    /// Run invariant checks. Returns the first violation
    /// found.
    pub fn validate(self) -> Result<(), TripleBufferConfigError> {
        if self.warn_after_ms == 0 {
            return Err(TripleBufferConfigError::WarnZero);
        }
        if self.force_recycle_after_ms == 0 {
            return Err(TripleBufferConfigError::ForceZero);
        }
        if self.force_recycle_after_ms <= self.warn_after_ms {
            return Err(TripleBufferConfigError::ForceNotGreaterThanWarn {
                warn_ms: self.warn_after_ms,
                force_ms: self.force_recycle_after_ms,
            });
        }
        if self.force_recycle_after_ms > MAX_FORCE_RECYCLE_MS {
            return Err(TripleBufferConfigError::ForceExceedsCap {
                force_ms: self.force_recycle_after_ms,
                cap_ms: MAX_FORCE_RECYCLE_MS,
            });
        }
        Ok(())
    }
}

/// Validation failure surfaced by
/// [`TripleBufferConfigSection::validate`] or
/// [`parse_render_triple_buffer_section`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TripleBufferConfigError {
    #[error("warn_after_ms must be > 0")]
    WarnZero,
    #[error("force_recycle_after_ms must be > 0")]
    ForceZero,
    #[error(
        "force_recycle_after_ms ({force_ms}) must be > warn_after_ms ({warn_ms})"
    )]
    ForceNotGreaterThanWarn { warn_ms: u64, force_ms: u64 },
    #[error("force_recycle_after_ms ({force_ms}) exceeds {cap_ms}ms cap")]
    ForceExceedsCap { force_ms: u64, cap_ms: u64 },
    #[error("toml parse error: {0}")]
    TomlParse(String),
}

/// Parse the full frankenterm.toml string and extract the
/// `[render.triple_buffer]` section.
///
/// Returns `Ok(None)` when the section is absent (operator
/// hasn't customized — integration uses
/// [`WatchdogConfig::default`]). Returns `Ok(Some(_))` after
/// successful parse + invariant validation. Returns
/// `Err(...)` on TOML syntax errors or invariant violations.
pub fn parse_render_triple_buffer_section(
    toml: &str,
) -> Result<Option<TripleBufferConfigSection>, TripleBufferConfigError> {
    #[derive(Deserialize)]
    struct RootDoc {
        render: Option<RenderDoc>,
    }
    #[derive(Deserialize)]
    struct RenderDoc {
        triple_buffer: Option<TripleBufferConfigSection>,
    }
    let doc: RootDoc = toml::from_str(toml)
        .map_err(|e| TripleBufferConfigError::TomlParse(e.to_string()))?;
    let Some(render) = doc.render else {
        return Ok(None);
    };
    let Some(section) = render.triple_buffer else {
        return Ok(None);
    };
    section.validate()?;
    Ok(Some(section))
}

/// Compose a [`TripleBufferReconfigureAuditEvent`] from the
/// pre/post watchdog configs and the operator's reload
/// timestamp.
#[must_use]
pub fn build_reconfigure_event(
    pane_id: PaneId,
    prev: WatchdogConfig,
    new_section: TripleBufferConfigSection,
    now_ms: u64,
    source: ReconfigureSource,
) -> TripleBufferReconfigureAuditEvent {
    let prev_section = TripleBufferConfigSection::from_watchdog_config(&prev);
    TripleBufferReconfigureAuditEvent {
        pane_id,
        prev_warn_ms: prev_section.warn_after_ms,
        prev_force_ms: prev_section.force_recycle_after_ms,
        new_warn_ms: new_section.warn_after_ms,
        new_force_ms: new_section.force_recycle_after_ms,
        timestamp_ms: now_ms,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sane_section() -> TripleBufferConfigSection {
        TripleBufferConfigSection {
            warn_after_ms: 1_000,
            force_recycle_after_ms: 5_000,
        }
    }

    #[test]
    fn validate_accepts_sane_section() {
        assert!(sane_section().validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_warn() {
        let s = TripleBufferConfigSection {
            warn_after_ms: 0,
            force_recycle_after_ms: 5_000,
        };
        assert_eq!(s.validate(), Err(TripleBufferConfigError::WarnZero));
    }

    #[test]
    fn validate_rejects_zero_force() {
        let s = TripleBufferConfigSection {
            warn_after_ms: 1_000,
            force_recycle_after_ms: 0,
        };
        assert_eq!(s.validate(), Err(TripleBufferConfigError::ForceZero));
    }

    #[test]
    fn validate_rejects_force_below_or_equal_warn() {
        let s = TripleBufferConfigSection {
            warn_after_ms: 1_000,
            force_recycle_after_ms: 1_000,
        };
        assert!(matches!(
            s.validate(),
            Err(TripleBufferConfigError::ForceNotGreaterThanWarn { .. })
        ));
        let s = TripleBufferConfigSection {
            warn_after_ms: 5_000,
            force_recycle_after_ms: 1_000,
        };
        assert!(matches!(
            s.validate(),
            Err(TripleBufferConfigError::ForceNotGreaterThanWarn { .. })
        ));
    }

    #[test]
    fn validate_rejects_force_above_60s_cap() {
        let s = TripleBufferConfigSection {
            warn_after_ms: 1_000,
            force_recycle_after_ms: 60_001,
        };
        assert!(matches!(
            s.validate(),
            Err(TripleBufferConfigError::ForceExceedsCap { .. })
        ));
    }

    #[test]
    fn parse_returns_section_when_present() {
        let toml_str = r#"
[render.triple_buffer]
warn_after_ms = 2000
force_recycle_after_ms = 8000
"#;
        let parsed = parse_render_triple_buffer_section(toml_str).unwrap();
        assert_eq!(
            parsed,
            Some(TripleBufferConfigSection {
                warn_after_ms: 2_000,
                force_recycle_after_ms: 8_000,
            })
        );
    }

    #[test]
    fn parse_returns_none_when_section_absent() {
        let toml_str = r#"
[render]
some_other_key = "value"
"#;
        assert_eq!(
            parse_render_triple_buffer_section(toml_str).unwrap(),
            None
        );
    }

    #[test]
    fn parse_returns_none_when_render_table_absent() {
        let toml_str = r#"
[other_section]
key = "value"
"#;
        assert_eq!(
            parse_render_triple_buffer_section(toml_str).unwrap(),
            None
        );
    }

    #[test]
    fn parse_surfaces_validation_error() {
        let toml_str = r#"
[render.triple_buffer]
warn_after_ms = 0
force_recycle_after_ms = 5000
"#;
        let err = parse_render_triple_buffer_section(toml_str).unwrap_err();
        assert_eq!(err, TripleBufferConfigError::WarnZero);
    }

    #[test]
    fn parse_surfaces_invariant_inversion() {
        let toml_str = r#"
[render.triple_buffer]
warn_after_ms = 5000
force_recycle_after_ms = 1000
"#;
        let err = parse_render_triple_buffer_section(toml_str).unwrap_err();
        assert!(matches!(
            err,
            TripleBufferConfigError::ForceNotGreaterThanWarn { .. }
        ));
    }

    #[test]
    fn parse_surfaces_toml_syntax_error() {
        let toml_str = "[render.triple_buffer\nwarn_after_ms = 1000";
        let err = parse_render_triple_buffer_section(toml_str).unwrap_err();
        assert!(matches!(err, TripleBufferConfigError::TomlParse(_)));
    }

    #[test]
    fn parse_rejects_section_missing_force_field() {
        let toml_str = r#"
[render.triple_buffer]
warn_after_ms = 1000
"#;
        let err = parse_render_triple_buffer_section(toml_str).unwrap_err();
        assert!(matches!(err, TripleBufferConfigError::TomlParse(_)));
    }

    #[test]
    fn watchdog_config_roundtrip_preserves_values() {
        let section = sane_section();
        let cfg = section.to_watchdog_config();
        let back = TripleBufferConfigSection::from_watchdog_config(&cfg);
        assert_eq!(back, section);
    }

    #[test]
    fn build_event_captures_pre_and_post_values() {
        let prev = TripleBufferConfigSection {
            warn_after_ms: 500,
            force_recycle_after_ms: 2_000,
        }
        .to_watchdog_config();
        let new = TripleBufferConfigSection {
            warn_after_ms: 1_500,
            force_recycle_after_ms: 6_000,
        };
        let event = build_reconfigure_event(
            PaneId(42),
            prev,
            new,
            123_456,
            ReconfigureSource::OperatorReload,
        );
        assert_eq!(event.pane_id, PaneId(42));
        assert_eq!(event.prev_warn_ms, 500);
        assert_eq!(event.prev_force_ms, 2_000);
        assert_eq!(event.new_warn_ms, 1_500);
        assert_eq!(event.new_force_ms, 6_000);
        assert_eq!(event.timestamp_ms, 123_456);
        assert_eq!(event.source, ReconfigureSource::OperatorReload);
        assert!(event.is_relaxed());
        assert!(!event.is_no_op());
    }

    #[test]
    fn build_event_no_op_when_values_unchanged() {
        let cfg = sane_section().to_watchdog_config();
        let event = build_reconfigure_event(
            PaneId(7),
            cfg,
            sane_section(),
            1_000,
            ReconfigureSource::OperatorReload,
        );
        assert!(event.is_no_op());
    }

    #[test]
    fn scenario_operator_reload_pipeline() {
        let toml_str = r#"
[render.triple_buffer]
warn_after_ms = 750
force_recycle_after_ms = 3000
"#;
        let new_section = parse_render_triple_buffer_section(toml_str)
            .expect("parse ok")
            .expect("section present");

        let prev = WatchdogConfig::default();
        let event = build_reconfigure_event(
            PaneId(1),
            prev,
            new_section,
            42_000,
            ReconfigureSource::OperatorReload,
        );

        assert_eq!(event.new_warn_ms, 750);
        assert_eq!(event.new_force_ms, 3_000);
        assert!(!event.is_relaxed());
        assert!(!event.is_no_op());
    }
}
