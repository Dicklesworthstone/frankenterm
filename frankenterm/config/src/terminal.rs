//! Bridge our gui config into the terminal crate configuration

use crate::{configuration, ConfigHandle, NewlineCanon};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::config::{
    BidiMode, RecoveryActivationLease, ScrollbackSpillSink, TerminalConfigurationRevision,
};
use frankenterm_term::MonospaceKpCostModel;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use termwiz::cell::UnicodeVersion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackSpillSinkContext {
    pub pane_id: usize,
    pub domain_id: usize,
    /// Random identity allocated before terminal construction. Unlike the
    /// process-local numeric pane ID, this value is safe to persist and can be
    /// retained by an external PTY guardian across mux incarnations.
    pub durable_pane_id: [u8; 16],
    pub command_description: String,
}

pub type ScrollbackSpillSinkFactory =
    dyn Fn(ScrollbackSpillSinkContext) -> Option<Arc<dyn ScrollbackSpillSink>> + Send + Sync;

fn scrollback_spill_sink_factory() -> &'static Mutex<Option<Arc<ScrollbackSpillSinkFactory>>> {
    static FACTORY: OnceLock<Mutex<Option<Arc<ScrollbackSpillSinkFactory>>>> = OnceLock::new();
    FACTORY.get_or_init(|| Mutex::new(None))
}

fn lock_terminal_mutex<'a, T>(mutex: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("recovering poisoned {name} mutex");
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

pub fn set_scrollback_spill_sink_factory(factory: Option<Arc<ScrollbackSpillSinkFactory>>) {
    *lock_terminal_mutex(
        scrollback_spill_sink_factory(),
        "scrollback spill sink factory",
    ) = factory;
}

fn scrollback_spill_sink_for(
    context: ScrollbackSpillSinkContext,
) -> Option<Arc<dyn ScrollbackSpillSink>> {
    let factory = lock_terminal_mutex(
        scrollback_spill_sink_factory(),
        "scrollback spill sink factory",
    )
    .clone();
    factory.and_then(|factory| factory(context))
}

#[derive(Debug)]
pub struct TermConfig {
    recovery_activation_gate: Mutex<()>,
    config: Mutex<Option<ConfigHandle>>,
    client_palette: Mutex<Option<ColorPalette>>,
    overlay_generation: AtomicUsize,
    scrollback_spill_sink: Option<Arc<dyn ScrollbackSpillSink>>,
}

struct TermConfigRecoveryActivationLease<'config> {
    _guard: MutexGuard<'config, ()>,
}

impl std::fmt::Debug for TermConfigRecoveryActivationLease<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TermConfigRecoveryActivationLease")
            .finish_non_exhaustive()
    }
}

impl RecoveryActivationLease for TermConfigRecoveryActivationLease<'_> {}

fn bump_overlay_generation(generation: &AtomicUsize) {
    let mut current = generation.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).unwrap_or_else(|| {
            panic!(
                "terminal configuration overlay generation exhausted; refusing to reuse an identity"
            )
        });
        match generation.compare_exchange_weak(current, next, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

impl TermConfig {
    pub fn new() -> Self {
        Self {
            recovery_activation_gate: Mutex::new(()),
            config: Mutex::new(None),
            client_palette: Mutex::new(None),
            overlay_generation: AtomicUsize::new(0),
            scrollback_spill_sink: None,
        }
    }

    pub fn new_for_pane(
        pane_id: usize,
        domain_id: usize,
        durable_pane_id: [u8; 16],
        command_description: impl Into<String>,
    ) -> Self {
        let command_description = command_description.into();
        let scrollback_spill_sink = scrollback_spill_sink_for(ScrollbackSpillSinkContext {
            pane_id,
            domain_id,
            durable_pane_id,
            command_description,
        });
        Self {
            recovery_activation_gate: Mutex::new(()),
            config: Mutex::new(None),
            client_palette: Mutex::new(None),
            overlay_generation: AtomicUsize::new(0),
            scrollback_spill_sink,
        }
    }

    pub fn with_config(config: ConfigHandle) -> Self {
        Self {
            recovery_activation_gate: Mutex::new(()),
            config: Mutex::new(Some(config)),
            client_palette: Mutex::new(None),
            overlay_generation: AtomicUsize::new(0),
            scrollback_spill_sink: None,
        }
    }

    pub fn with_config_and_scrollback_spill_sink(
        config: ConfigHandle,
        scrollback_spill_sink: Option<Arc<dyn ScrollbackSpillSink>>,
    ) -> Self {
        Self {
            recovery_activation_gate: Mutex::new(()),
            config: Mutex::new(Some(config)),
            client_palette: Mutex::new(None),
            overlay_generation: AtomicUsize::new(0),
            scrollback_spill_sink,
        }
    }

    pub fn set_config(&self, config: ConfigHandle) {
        let _activation = lock_terminal_mutex(
            &self.recovery_activation_gate,
            "terminal recovery activation gate",
        );
        let mut current = lock_terminal_mutex(&self.config, "terminal config");
        bump_overlay_generation(&self.overlay_generation);
        let _previous = current.replace(config);
    }

    pub fn set_client_palette(&self, palette: ColorPalette) {
        let _activation = lock_terminal_mutex(
            &self.recovery_activation_gate,
            "terminal recovery activation gate",
        );
        let mut current = lock_terminal_mutex(&self.client_palette, "terminal client palette");
        bump_overlay_generation(&self.overlay_generation);
        let _previous = current.replace(palette);
    }

    fn configuration(&self) -> ConfigHandle {
        if let Some(handle) = lock_terminal_mutex(&self.config, "terminal config")
            .as_ref()
            .cloned()
        {
            handle
        } else {
            configuration()
        }
    }
}

impl frankenterm_term::TerminalConfiguration for TermConfig {
    fn osc52_write_policy(&self) -> frankenterm_term::config::Osc52WritePolicy {
        self.configuration().osc52_write_policy
    }

    fn osc52_write_max_bytes(&self) -> usize {
        self.configuration().osc52_write_max_bytes
    }

    fn generation(&self) -> usize {
        self.configuration().generation()
    }

    fn revision(&self) -> TerminalConfigurationRevision {
        loop {
            let overlay_generation = self.overlay_generation.load(Ordering::Acquire);
            let base_generation = self.configuration().generation();
            if self.overlay_generation.load(Ordering::Acquire) == overlay_generation {
                return TerminalConfigurationRevision::new(base_generation, overlay_generation);
            }
        }
    }

    fn acquire_recovery_activation_lease(&self) -> Box<dyn RecoveryActivationLease + '_> {
        let activation = lock_terminal_mutex(
            &self.recovery_activation_gate,
            "terminal recovery activation gate",
        );

        // A pane created before its first explicit config assignment otherwise
        // consults the reloadable global on every getter. Pin the exact current
        // handle while the mutation gate is held so the lease also excludes a
        // global reload from changing semantics between verification and
        // installation. This does not bump the overlay revision because the
        // effective value is unchanged at the pinning instant.
        let mut current = lock_terminal_mutex(&self.config, "terminal config");
        if current.is_none() {
            *current = Some(configuration());
        }
        drop(current);

        Box::new(TermConfigRecoveryActivationLease { _guard: activation })
    }

    fn scrollback_size(&self) -> usize {
        self.configuration().scrollback_lines
    }

    fn scrollback_tier_config(&self) -> frankenterm_term::config::ScrollbackTierConfig {
        let config = self.configuration();
        let hot_lines = config.scrollback_hot_lines.min(config.scrollback_lines);
        let warm_max_bytes = config.scrollback_warm_max_mb.saturating_mul(1024 * 1024);
        frankenterm_term::config::ScrollbackTierConfig {
            enabled: config.scrollback_tiered_enabled,
            hot_lines,
            warm_max_bytes,
        }
    }

    fn scrollback_spill_sink(&self) -> Option<Arc<dyn ScrollbackSpillSink>> {
        self.scrollback_spill_sink.clone()
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
        let client_palette = lock_terminal_mutex(&self.client_palette, "terminal client palette");
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

    fn enable_checksum_rectangular_area(&self) -> bool {
        self.configuration().enable_checksum_rectangular_area
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

    fn max_user_vars(&self) -> usize {
        self.configuration().max_user_vars
    }

    fn max_unicode_version_stack_depth(&self) -> usize {
        self.configuration().max_unicode_version_stack_depth
    }

    fn max_accumulating_title_len(&self) -> usize {
        self.configuration().max_accumulating_title_len
    }

    fn max_color_map_entries(&self) -> usize {
        self.configuration().max_color_map_entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_dynamic::Value;
    use frankenterm_term::Line;
    use frankenterm_term::TerminalConfiguration;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[derive(Debug)]
    struct TestScrollbackSpillSink;

    impl ScrollbackSpillSink for TestScrollbackSpillSink {
        fn store_scrollback_line(
            &self,
            _stable_row: frankenterm_term::StableRowIndex,
            _line: &Line,
            _max_retained_rows: usize,
        ) -> bool {
            true
        }

        fn load_scrollback_line(
            &self,
            _stable_row: frankenterm_term::StableRowIndex,
        ) -> Option<Line> {
            None
        }

        fn oldest_scrollback_row(&self) -> Option<frankenterm_term::StableRowIndex> {
            Some(0)
        }

        fn retained_scrollback_rows(&self) -> usize {
            1
        }

        fn retained_scrollback_bytes(&self) -> usize {
            1
        }

        fn snapshot_scrollback(
            &self,
            _expected_newest_exclusive: frankenterm_term::StableRowIndex,
            _limits: frankenterm_term::config::ScrollbackSnapshotLimits,
        ) -> Result<
            frankenterm_term::config::ScrollbackSnapshot,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            Err(frankenterm_term::config::ScrollbackSpillError::StorageUnavailable)
        }

        fn replace_scrollback_prefix(
            &self,
            _expected_generation: Option<frankenterm_term::config::ScrollbackSnapshotGeneration>,
            _prefix: frankenterm_term::config::ScrollbackPrefix<'_>,
            _max_retained_rows: usize,
        ) -> Result<
            frankenterm_term::config::ScrollbackReplaceCommit,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            Err(frankenterm_term::config::ScrollbackSpillError::StorageUnavailable)
        }

        fn clear_scrollback(
            &self,
        ) -> Result<
            frankenterm_term::config::ScrollbackClearCommit,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            Ok(frankenterm_term::config::ScrollbackClearCommit::new(
                frankenterm_term::config::ScrollbackSnapshotGeneration::new([0; 16], 0),
            ))
        }
    }

    fn overridden_config_for_test(overrides: Value) -> ConfigHandle {
        let _env_lock = crate::test_env_lock();
        crate::overridden_config(&overrides).expect("override parsing to succeed")
    }

    #[test]
    fn osc52_parsed_native_config_reaches_terminal_and_revokes_consent() {
        use frankenterm_term::{
            Clipboard, ClipboardSelection, Osc52ClipboardRequest, Terminal, TerminalSize,
        };
        #[derive(Default)]
        struct Sink {
            effects: Mutex<Vec<(ClipboardSelection, Option<String>)>>,
            prompt: Mutex<Option<Osc52ClipboardRequest>>,
        }
        impl Clipboard for Sink {
            fn set_contents(
                &self,
                selection: ClipboardSelection,
                data: Option<String>,
            ) -> anyhow::Result<()> {
                self.effects.lock().unwrap().push((selection, data));
                Ok(())
            }
            fn request_contents(&self, request: Osc52ClipboardRequest) -> anyhow::Result<()> {
                *self.prompt.lock().unwrap() = Some(request);
                Ok(())
            }
        }
        assert_eq!(
            ConfigHandle::default_config().osc52_write_policy,
            frankenterm_term::config::Osc52WritePolicy::Prompt
        );
        assert_eq!(
            ConfigHandle::default_config().osc52_write_max_bytes,
            1024 * 1024
        );
        for policy in ["Allow", "Deny", "Prompt"] {
            for cap in [1, 2] {
                let mut overrides = BTreeMap::new();
                overrides.insert(
                    Value::String("osc52_write_policy".into()),
                    Value::String(policy.into()),
                );
                overrides.insert(
                    Value::String("osc52_write_max_bytes".into()),
                    Value::U64(cap),
                );
                let config = Arc::new(TermConfig::with_config(overridden_config_for_test(
                    Value::Object(overrides.into()),
                )));
                let mut terminal = Terminal::new(
                    TerminalSize {
                        rows: 4,
                        cols: 32,
                        pixel_width: 0,
                        pixel_height: 0,
                        dpi: 96,
                    },
                    config.clone(),
                    "FrankenTerm",
                    "osc52-config-test",
                    Box::new(std::io::sink()),
                );
                let sink = Arc::new(Sink::default());
                let clipboard: Arc<dyn Clipboard> = sink.clone();
                terminal.set_clipboard(&clipboard);
                terminal.advance_bytes(b"\x1b]52;c;w6k=\x1b\\");
                assert_eq!(
                    sink.effects.lock().unwrap().len(),
                    usize::from(policy == "Allow" && cap == 2)
                );
                let request = sink.prompt.lock().unwrap().take();
                assert_eq!(request.is_some(), policy == "Prompt" && cap == 2);
                if let Some(request) = request {
                    assert!(
                        sink.effects.lock().unwrap().is_empty(),
                        "zero effects before operator consent"
                    );
                    request
                        .apply_with(|selection, data| sink.set_contents(selection, data))
                        .unwrap();
                    assert_eq!(
                        sink.effects.lock().unwrap().as_slice(),
                        &[(ClipboardSelection::Clipboard, Some("é".to_string()))]
                    );
                    assert_eq!(
                        request.apply_with(|_, _| panic!("duplicate consent reached sink")),
                        Err(frankenterm_term::Osc52PromptError::AlreadyResolved)
                    );
                    terminal.advance_bytes(b"\x1b]52;c;w6k=\x1b\\");
                    let revoked = sink.prompt.lock().unwrap().take().unwrap();
                    config.set_config(ConfigHandle::default_config());
                    assert_eq!(
                        revoked.apply_with(|_, _| panic!("revoked config reached sink")),
                        Err(frankenterm_term::Osc52PromptError::Revoked)
                    );
                    assert_eq!(sink.effects.lock().unwrap().len(), 1);
                }
                println!(
                    "OSC52_CONFIG_TERMINAL policy={policy} cap={cap} effects={}",
                    sink.effects.lock().unwrap().len()
                );
            }
        }
    }

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
        overrides.insert(Value::String("scrollback_lines".into()), Value::U64(5000));
        overrides.insert(
            Value::String("scrollback_tiered_enabled".into()),
            Value::Bool(true),
        );
        overrides.insert(
            Value::String("scrollback_hot_lines".into()),
            Value::U64(1200),
        );
        overrides.insert(
            Value::String("scrollback_warm_max_mb".into()),
            Value::U64(64),
        );

        let handle = overridden_config_for_test(Value::Object(overrides.into()));
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
        let tier = term_config.scrollback_tier_config();
        assert!(tier.enabled);
        assert_eq!(tier.hot_lines, 1200);
        assert_eq!(tier.warm_max_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn term_config_new_for_pane_uses_registered_scrollback_sink_factory() {
        let _env_lock = crate::test_env_lock();
        set_scrollback_spill_sink_factory(Some(Arc::new(|context| {
            assert_eq!(context.pane_id, 7);
            assert_eq!(context.domain_id, 3);
            assert_eq!(context.durable_pane_id, [7; 16]);
            assert_eq!(context.command_description, "shell");
            Some(Arc::new(TestScrollbackSpillSink))
        })));

        let term_config = TermConfig::new_for_pane(7, 3, [7; 16], "shell");
        let sink = term_config
            .scrollback_spill_sink()
            .expect("factory should provide a sink");
        assert_eq!(sink.retained_scrollback_rows(), 1);

        set_scrollback_spill_sink_factory(None);
    }

    #[test]
    fn term_config_recovers_after_poisoned_locks() {
        let _env_lock = crate::test_env_lock();

        let factory_poisoned = std::panic::catch_unwind(|| {
            let _factory = scrollback_spill_sink_factory()
                .lock()
                .expect("factory lock for poison test");
            panic!("poison scrollback sink factory");
        });
        assert!(factory_poisoned.is_err());

        set_scrollback_spill_sink_factory(Some(Arc::new(|_context| {
            Some(Arc::new(TestScrollbackSpillSink))
        })));
        let term_config = TermConfig::new_for_pane(9, 4, [9; 16], "poisoned-shell");
        assert!(term_config.scrollback_spill_sink().is_some());
        set_scrollback_spill_sink_factory(None);

        let term_config = TermConfig::new();
        let config_poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _config = term_config
                .config
                .lock()
                .expect("config lock for poison test");
            panic!("poison terminal config");
        }));
        assert!(config_poisoned.is_err());

        let handle = ConfigHandle::default_config();
        term_config.set_config(handle.clone());
        assert_eq!(term_config.generation(), handle.generation());

        let palette_poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _palette = term_config
                .client_palette
                .lock()
                .expect("client palette lock for poison test");
            panic!("poison terminal client palette");
        }));
        assert!(palette_poisoned.is_err());

        let palette = ColorPalette::default();
        term_config.set_client_palette(palette.clone());
        assert_eq!(term_config.color_palette(), palette);
    }

    #[test]
    fn term_config_revision_tracks_same_generation_overlays() {
        let handle = ConfigHandle::default_config();
        let term_config = TermConfig::with_config(handle.clone());
        let initial = term_config.revision();

        term_config.set_config(handle);
        let after_same_generation_config_swap = term_config.revision();
        assert_ne!(after_same_generation_config_swap, initial);

        term_config.set_client_palette(ColorPalette::default());
        assert_ne!(term_config.revision(), after_same_generation_config_swap);
    }

    #[test]
    fn recovery_activation_lease_excludes_every_overlay_setter() {
        use std::sync::TryLockError;
        use std::time::{Duration, Instant};

        fn wait_until_setter_owns_activation_gate(term_config: &TermConfig) {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match term_config.recovery_activation_gate.try_lock() {
                    Err(TryLockError::WouldBlock) => return,
                    Err(TryLockError::Poisoned(_)) => {
                        panic!("terminal recovery activation gate was poisoned")
                    }
                    Ok(guard) => drop(guard),
                }
                assert!(
                    Instant::now() < deadline,
                    "setter never acquired the shared recovery activation gate",
                );
                std::thread::yield_now();
            }
        }

        let term_config = Arc::new(TermConfig::with_config(ConfigHandle::default_config()));

        let revision_before_config = term_config.overlay_generation.load(Ordering::Relaxed);
        let lease = term_config.acquire_recovery_activation_lease();
        let config_guard = lock_terminal_mutex(&term_config.config, "config setter exclusion test");
        let config_setter_config = Arc::clone(&term_config);
        let config_setter = std::thread::spawn(move || {
            config_setter_config.set_config(ConfigHandle::default_config());
        });
        assert_eq!(
            term_config.overlay_generation.load(Ordering::Relaxed),
            revision_before_config,
            "config mutation must not cross the held activation lease",
        );
        drop(lease);
        wait_until_setter_owns_activation_gate(&term_config);
        assert_eq!(
            term_config.overlay_generation.load(Ordering::Relaxed),
            revision_before_config,
            "config revision cannot advance before the protected value is mutable",
        );
        drop(config_guard);
        config_setter.join().expect("config setter thread joins");

        let revision_before_palette = term_config.overlay_generation.load(Ordering::Relaxed);
        let lease = term_config.acquire_recovery_activation_lease();
        let palette_guard =
            lock_terminal_mutex(&term_config.client_palette, "palette setter exclusion test");
        let palette_config = Arc::clone(&term_config);
        let palette_setter = std::thread::spawn(move || {
            palette_config.set_client_palette(ColorPalette::default());
        });
        assert_eq!(
            term_config.overlay_generation.load(Ordering::Relaxed),
            revision_before_palette,
            "palette mutation must not cross the held activation lease",
        );
        drop(lease);
        wait_until_setter_owns_activation_gate(&term_config);
        assert_eq!(
            term_config.overlay_generation.load(Ordering::Relaxed),
            revision_before_palette,
            "palette revision cannot advance before the protected value is mutable",
        );
        drop(palette_guard);
        palette_setter.join().expect("palette setter thread joins");
    }

    #[test]
    fn exhausted_overlay_revision_refuses_mutation_before_value_changes() {
        let term_config = TermConfig::new();
        term_config
            .overlay_generation
            .store(usize::MAX, Ordering::Relaxed);

        let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            term_config.set_client_palette(ColorPalette::default());
        }));
        assert!(attempted.is_err());
        assert!(
            lock_terminal_mutex(
                &term_config.client_palette,
                "terminal client palette exhaustion test",
            )
            .is_none(),
            "revision exhaustion must leave the prior semantic value untouched",
        );
    }

    #[test]
    fn term_config_maps_terminal_state_limits() {
        let mut overrides = BTreeMap::new();
        overrides.insert(Value::String("max_user_vars".into()), Value::U64(1024));
        overrides.insert(
            Value::String("max_unicode_version_stack_depth".into()),
            Value::U64(128),
        );
        overrides.insert(
            Value::String("max_accumulating_title_len".into()),
            Value::U64(16384),
        );
        overrides.insert(
            Value::String("max_color_map_entries".into()),
            Value::U64(8192),
        );

        let handle = overridden_config_for_test(Value::Object(overrides.into()));
        let term_config = TermConfig::with_config(handle);

        assert_eq!(term_config.max_user_vars(), 1024);
        assert_eq!(term_config.max_unicode_version_stack_depth(), 128);
        assert_eq!(term_config.max_accumulating_title_len(), 16384);
        assert_eq!(term_config.max_color_map_entries(), 8192);
    }

    #[test]
    fn term_config_defaults_match_original_constants() {
        let handle = ConfigHandle::default_config();
        let term_config = TermConfig::with_config(handle);

        assert_eq!(term_config.max_user_vars(), 512);
        assert_eq!(term_config.max_unicode_version_stack_depth(), 64);
        assert_eq!(term_config.max_accumulating_title_len(), 8192);
        assert_eq!(term_config.max_color_map_entries(), 4096);
    }

    #[test]
    fn mux_config_defaults_match_original_constants() {
        let handle = ConfigHandle::default_config();
        assert_eq!(handle.mux_socket_buffer_size, 1024 * 1024);
        assert_eq!(handle.mux_max_synchronized_output_bytes, 8 * 1024 * 1024);
        assert_eq!(handle.mux_tmux_max_backlog_bytes_per_pane, 1_048_576);
        assert_eq!(handle.mux_tmux_max_backlog_bytes, 32 * 1024 * 1024);
        assert_eq!(handle.mux_tmux_max_backlog_entries, 1024);
        assert_eq!(handle.mux_tmux_max_backlog_items, 16_384);
        assert_eq!(handle.mux_tmux_backlog_expiry_ms, 30_000);
        assert_eq!(handle.mux_tmux_max_output_queue_items_per_pane, 1024);
        assert_eq!(handle.mux_tmux_output_write_quantum_bytes, 256 * 1024);
        assert_eq!(handle.mux_tmux_io_start_timeout_ms, 500);
        assert_eq!(handle.mux_tmux_io_write_timeout_ms, 2_000);
        assert_eq!(handle.mux_tmux_response_timeout_ms, 10_000);
    }

    #[test]
    fn floating_pane_defaults_match_original_constants() {
        let handle = ConfigHandle::default_config();
        assert_eq!(handle.min_floating_pane_width, 5);
        assert_eq!(handle.min_floating_pane_height, 3);
    }

    #[test]
    fn timeout_defaults_match_original_constants() {
        let handle = ConfigHandle::default_config();
        assert_eq!(handle.ssh_initial_poll_delay_ms, 100);
        assert_eq!(handle.ssh_max_poll_delay_ms, 2000);
        assert_eq!(handle.client_reconnect_base_interval_ms, 1000);
        assert_eq!(handle.client_reconnect_max_interval_ms, 10000);
        assert_eq!(handle.client_reconnect_max_attempts, 0);
        assert_eq!(handle.client_reconnect_healthy_session_ms, 30000);
        assert_eq!(handle.render_base_poll_interval_ms, 20);
        assert_eq!(handle.render_max_poll_interval_ms, 30000);
        assert_eq!(handle.connui_poll_timeout_ms, 200);
        assert_eq!(handle.ssh_terminal_poll_timeout_ms, 200);
    }
}
