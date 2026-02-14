//! Bridge our gui config into the terminal crate configuration

use crate::{ConfigHandle, NewlineCanon, configuration};
use frankenterm_term::MonospaceKpCostModel;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::config::BidiMode;
use std::sync::Mutex;
use termwiz::cell::UnicodeVersion;

#[derive(Debug)]
pub struct TermConfig {
    config: Mutex<Option<ConfigHandle>>,
    client_palette: Mutex<Option<ColorPalette>>,
}

impl TermConfig {
    pub fn new() -> Self {
        Self {
            config: Mutex::new(None),
            client_palette: Mutex::new(None),
        }
    }

    pub fn with_config(config: ConfigHandle) -> Self {
        Self {
            config: Mutex::new(Some(config)),
            client_palette: Mutex::new(None),
        }
    }

    pub fn set_config(&self, config: ConfigHandle) {
        self.config.lock().unwrap().replace(config);
    }

    pub fn set_client_palette(&self, palette: ColorPalette) {
        self.client_palette.lock().unwrap().replace(palette);
    }

    fn configuration(&self) -> ConfigHandle {
        match self.config.lock().unwrap().as_ref() {
            Some(h) => h.clone(),
            None => configuration(),
        }
    }
}

impl frankenterm_term::TerminalConfiguration for TermConfig {
    fn generation(&self) -> usize {
        self.configuration().generation()
    }

    fn scrollback_size(&self) -> usize {
        self.configuration().scrollback_lines
    }

    fn resize_wrap_kp_cost_model(&self) -> MonospaceKpCostModel {
        let config = self.configuration();
        MonospaceKpCostModel {
            badness_scale: config.resize_wrap_kp_badness_scale,
            forced_break_penalty: config.resize_wrap_kp_forced_break_penalty,
            lookahead_limit: config.resize_wrap_kp_lookahead_limit,
            max_dp_states: config.resize_wrap_kp_max_dp_states,
        }
    }

    fn resize_wrap_scorecard_enabled(&self) -> bool {
        self.configuration().resize_wrap_scorecard_enabled
    }

    fn resize_wrap_readability_gate_enabled(&self) -> bool {
        self.configuration().resize_wrap_readability_gate_enabled
    }

    fn resize_wrap_readability_max_line_badness_delta(&self) -> i64 {
        self.configuration()
            .resize_wrap_readability_max_line_badness_delta
    }

    fn resize_wrap_readability_max_total_badness_delta(&self) -> i64 {
        self.configuration()
            .resize_wrap_readability_max_total_badness_delta
    }

    fn resize_wrap_readability_max_fallback_ratio_percent(&self) -> u8 {
        self.configuration()
            .resize_wrap_readability_max_fallback_ratio_percent
    }

    fn enable_csi_u_key_encoding(&self) -> bool {
        self.configuration().enable_csi_u_key_encoding
    }

    fn color_palette(&self) -> ColorPalette {
        let client_palette = self.client_palette.lock().unwrap();
        if let Some(p) = client_palette.as_ref().cloned() {
            return p;
        }
        let config = self.configuration();

        config.resolved_palette.clone().into()
    }

    fn alternate_buffer_wheel_scroll_speed(&self) -> u8 {
        self.configuration().alternate_buffer_wheel_scroll_speed
    }

    fn enq_answerback(&self) -> String {
        configuration().enq_answerback.clone()
    }

    fn enable_kitty_graphics(&self) -> bool {
        self.configuration().enable_kitty_graphics
    }

    fn enable_title_reporting(&self) -> bool {
        self.configuration().enable_title_reporting
    }

    fn enable_kitty_keyboard(&self) -> bool {
        self.configuration().enable_kitty_keyboard
    }

    fn canonicalize_pasted_newlines(&self) -> frankenterm_term::config::NewlineCanon {
        match self.configuration().canonicalize_pasted_newlines {
            None => frankenterm_term::config::NewlineCanon::default(),
            Some(NewlineCanon::None) => frankenterm_term::config::NewlineCanon::None,
            Some(NewlineCanon::LineFeed) => frankenterm_term::config::NewlineCanon::LineFeed,
            Some(NewlineCanon::CarriageReturn) => {
                frankenterm_term::config::NewlineCanon::CarriageReturn
            }
            Some(NewlineCanon::CarriageReturnAndLineFeed) => {
                frankenterm_term::config::NewlineCanon::CarriageReturnAndLineFeed
            }
        }
    }

    fn unicode_version(&self) -> UnicodeVersion {
        let config = self.configuration();
        config.unicode_version()
    }

    fn debug_key_events(&self) -> bool {
        self.configuration().debug_key_events
    }

    fn log_unknown_escape_sequences(&self) -> bool {
        self.configuration().log_unknown_escape_sequences
    }

    fn normalize_output_to_unicode_nfc(&self) -> bool {
        self.configuration().normalize_output_to_unicode_nfc
    }

    fn bidi_mode(&self) -> BidiMode {
        let config = self.configuration();
        BidiMode {
            enabled: config.bidi_enabled,
            hint: config.bidi_direction,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_dynamic::Value;
    use frankenterm_term::TerminalConfiguration;
    use std::collections::BTreeMap;

    #[test]
    fn term_config_maps_resize_wrap_controls_from_config_handle() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            Value::String("resize_wrap_kp_badness_scale".into()),
            Value::U64(42_000),
        );
        overrides.insert(
            Value::String("resize_wrap_kp_forced_break_penalty".into()),
            Value::U64(7_500),
        );
        overrides.insert(
            Value::String("resize_wrap_kp_lookahead_limit".into()),
            Value::U64(24),
        );
        overrides.insert(
            Value::String("resize_wrap_kp_max_dp_states".into()),
            Value::U64(2_048),
        );
        overrides.insert(
            Value::String("resize_wrap_scorecard_enabled".into()),
            Value::Bool(true),
        );
        overrides.insert(
            Value::String("resize_wrap_readability_gate_enabled".into()),
            Value::Bool(true),
        );
        overrides.insert(
            Value::String("resize_wrap_readability_max_line_badness_delta".into()),
            Value::I64(12_345),
        );
        overrides.insert(
            Value::String("resize_wrap_readability_max_total_badness_delta".into()),
            Value::I64(67_890),
        );
        overrides.insert(
            Value::String("resize_wrap_readability_max_fallback_ratio_percent".into()),
            Value::U64(37),
        );

        let handle = crate::overridden_config(&Value::Object(overrides.into()))
            .expect("override parsing to succeed");
        let term_config = TermConfig::with_config(handle);

        let model = term_config.resize_wrap_kp_cost_model();
        assert_eq!(model.badness_scale, 42_000);
        assert_eq!(model.forced_break_penalty, 7_500);
        assert_eq!(model.lookahead_limit, 24);
        assert_eq!(model.max_dp_states, 2_048);
        assert!(term_config.resize_wrap_scorecard_enabled());
        assert!(term_config.resize_wrap_readability_gate_enabled());
        assert_eq!(
            term_config.resize_wrap_readability_max_line_badness_delta(),
            12_345
        );
        assert_eq!(
            term_config.resize_wrap_readability_max_total_badness_delta(),
            67_890
        );
        assert_eq!(
            term_config.resize_wrap_readability_max_fallback_ratio_percent(),
            37
        );
    }
}
