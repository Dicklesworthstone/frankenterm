//! Secret redaction primitives shared by FrankenTerm output surfaces.

pub(crate) mod hot_path_metrics {
    pub struct HotPathTimer;

    impl HotPathTimer {
        #[must_use]
        pub const fn start(_name: &'static str) -> Self {
            Self
        }
    }
}

#[path = "../../frankenterm-core/src/redactor.rs"]
pub mod redactor;

pub use redactor::*;

/// Redact any secret-looking text before it leaves a robot, CLI, MCP, or audit
/// response surface.
#[must_use]
pub fn redact_for_output(text: &str) -> String {
    Redactor::new().redact(text)
}

/// Redact caller-supplied wait patterns before echoing them in result or error
/// envelopes. Matching must continue to use the original pattern.
#[must_use]
pub fn redact_wait_pattern_for_output(pattern: &str) -> String {
    redact_for_output(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_pattern_output_redaction_masks_token_like_values() -> Result<(), String> {
        let probe = [
            "sk-",
            "ant-",
            "api03-",
            "abcdefghijklmnopqrstuvwxyz",
            "12345678901234567890",
        ]
        .concat();

        let redacted = redact_wait_pattern_for_output(&format!("ready {probe}"));

        if redacted.contains(&probe) {
            return Err("raw token-like probe leaked in wait-for pattern output".to_owned());
        }
        if !redacted.contains(REDACTED_MARKER) {
            return Err("expected redaction marker in wait-for pattern output".to_owned());
        }

        Ok(())
    }
}
