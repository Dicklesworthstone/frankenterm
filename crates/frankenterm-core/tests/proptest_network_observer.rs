//! Property-based tests for `network_observer` — rano-based connection attribution.

use proptest::prelude::*;

use frankenterm_core::network_observer::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_attribution() -> impl Strategy<Value = NetworkAttribution> {
    (
        "[a-zA-Z]{2,15}",
        prop::option::of("[a-z-]{2,15}"),
        0.0..2000.0f64,
        any::<bool>(),
        "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
    )
        .prop_map(|(provider, region, latency_ms, is_trusted, remote_addr)| {
            NetworkAttribution {
                provider,
                region,
                latency_ms,
                is_trusted,
                remote_addr,
                asn: None,
                org: None,
            }
        })
}

fn arb_connectivity_status() -> impl Strategy<Value = ConnectivityStatus> {
    prop_oneof![
        Just(ConnectivityStatus::Connected),
        Just(ConnectivityStatus::Degraded),
        Just(ConnectivityStatus::Unreachable),
        Just(ConnectivityStatus::Unknown),
    ]
}

fn arb_pressure_tier() -> impl Strategy<Value = NetworkPressureTier> {
    prop_oneof![
        Just(NetworkPressureTier::Green),
        Just(NetworkPressureTier::Yellow),
        Just(NetworkPressureTier::Red),
        Just(NetworkPressureTier::Black),
    ]
}

fn arb_config() -> impl Strategy<Value = NetworkObserverConfig> {
    (1.0..500.0f64, 500.0..2000.0f64, 1..120u64).prop_map(
        |(yellow, red, timeout)| NetworkObserverConfig {
            yellow_latency_ms: yellow,
            red_latency_ms: yellow.max(red), // Ensure red >= yellow
            timeout_secs: timeout,
        },
    )
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. NetworkAttribution serde roundtrip
    #[test]
    fn attribution_serde_roundtrip(attr in arb_attribution()) {
        let json_str = serde_json::to_string(&attr).unwrap();
        let rt: NetworkAttribution = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&attr.provider, &rt.provider);
        prop_assert_eq!(attr.region, rt.region);
        prop_assert!((attr.latency_ms - rt.latency_ms).abs() < 0.001);
        prop_assert_eq!(attr.is_trusted, rt.is_trusted);
    }

    // 2. NetworkAttribution None fields not serialized
    #[test]
    fn attribution_skip_none_fields(attr in arb_attribution()) {
        let json_str = serde_json::to_string(&attr).unwrap();
        if attr.region.is_none() {
            prop_assert!(!json_str.contains("\"region\""));
        }
        if attr.asn.is_none() {
            prop_assert!(!json_str.contains("\"asn\""));
        }
    }

    // 3. ConnectivityStatus serde roundtrip
    #[test]
    fn connectivity_serde_roundtrip(status in arb_connectivity_status()) {
        let json_str = serde_json::to_string(&status).unwrap();
        let rt: ConnectivityStatus = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(status, rt);
    }

    // 4. ConnectivityStatus display never empty
    #[test]
    fn connectivity_display_non_empty(status in arb_connectivity_status()) {
        let display = status.to_string();
        prop_assert!(!display.is_empty());
    }

    // 5. NetworkPressureTier serde roundtrip
    #[test]
    fn pressure_tier_serde_roundtrip(tier in arb_pressure_tier()) {
        let json_str = serde_json::to_string(&tier).unwrap();
        let rt: NetworkPressureTier = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(tier, rt);
    }

    // 6. NetworkPressureTier ordering is total
    #[test]
    fn pressure_tier_ordering_total(a in arb_pressure_tier(), b in arb_pressure_tier()) {
        // Total ordering: either a <= b or b <= a
        prop_assert!(a <= b || b <= a);
    }

    // 7. NetworkPressureTier Green is minimum
    #[test]
    fn pressure_tier_green_minimum(tier in arb_pressure_tier()) {
        prop_assert!(NetworkPressureTier::Green <= tier);
    }

    // 8. NetworkPressureTier Black is maximum
    #[test]
    fn pressure_tier_black_maximum(tier in arb_pressure_tier()) {
        prop_assert!(tier <= NetworkPressureTier::Black);
    }

    // 9. NetworkObserverConfig serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json_str = serde_json::to_string(&config).unwrap();
        let rt: NetworkObserverConfig = serde_json::from_str(&json_str).unwrap();
        prop_assert!((config.yellow_latency_ms - rt.yellow_latency_ms).abs() < 0.001);
        prop_assert!((config.red_latency_ms - rt.red_latency_ms).abs() < 0.001);
        prop_assert_eq!(config.timeout_secs, rt.timeout_secs);
    }

    // 10. NetworkObserverConfig default values
    #[test]
    fn config_defaults(_dummy in 0..1u8) {
        let config = NetworkObserverConfig::default();
        prop_assert!((config.yellow_latency_ms - 100.0).abs() < 0.001);
        prop_assert!((config.red_latency_ms - 500.0).abs() < 0.001);
        prop_assert_eq!(config.timeout_secs, 10);
    }

    // 11. classify_pressure: low latency → Green
    #[test]
    fn classify_pressure_low_is_green(latency_ms in 0.0..99.99f64) {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        prop_assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Green);
    }

    // 12. classify_pressure: medium latency → Yellow
    #[test]
    fn classify_pressure_medium_is_yellow(latency_ms in 100.0..499.99f64) {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        prop_assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Yellow);
    }

    // 13. classify_pressure: high latency → Red
    #[test]
    fn classify_pressure_high_is_red(latency_ms in 500.0..5000.0f64) {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        prop_assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Red);
    }

    // 14. classify_pressure monotonic: higher latency → same or worse tier
    #[test]
    fn classify_pressure_monotonic(low in 0.0..1000.0f64, delta in 0.0..1000.0f64) {
        let obs = NetworkObserver::new();
        let high = low + delta;
        let make_attr = |lat: f64| NetworkAttribution {
            provider: "t".into(),
            region: None,
            latency_ms: lat,
            is_trusted: false,
            remote_addr: "x".into(),
            asn: None,
            org: None,
        };
        let tier_low = obs.classify_pressure(&make_attr(low));
        let tier_high = obs.classify_pressure(&make_attr(high));
        prop_assert!(tier_low <= tier_high);
    }

    // 15. classify_pressure custom thresholds respected
    #[test]
    fn classify_pressure_custom_thresholds(
        yellow in 10.0..200.0f64,
        red_delta in 10.0..500.0f64,
    ) {
        let red = yellow + red_delta;
        let obs = NetworkObserver::with_config(NetworkObserverConfig {
            yellow_latency_ms: yellow,
            red_latency_ms: red,
            timeout_secs: 10,
        });
        let below_yellow = NetworkAttribution {
            provider: "t".into(),
            region: None,
            latency_ms: yellow - 1.0,
            is_trusted: false,
            remote_addr: "x".into(),
            asn: None,
            org: None,
        };
        prop_assert_eq!(obs.classify_pressure(&below_yellow), NetworkPressureTier::Green);
    }

    // 16. classify_connectivity Connected → Green
    #[test]
    fn classify_connectivity_connected(_dummy in 0..1u8) {
        let obs = NetworkObserver::new();
        prop_assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Connected),
            NetworkPressureTier::Green
        );
    }

    // 17. classify_connectivity Degraded → Yellow
    #[test]
    fn classify_connectivity_degraded(_dummy in 0..1u8) {
        let obs = NetworkObserver::new();
        prop_assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Degraded),
            NetworkPressureTier::Yellow
        );
    }

    // 18. classify_connectivity Unreachable → Black
    #[test]
    fn classify_connectivity_unreachable(_dummy in 0..1u8) {
        let obs = NetworkObserver::new();
        prop_assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Unreachable),
            NetworkPressureTier::Black
        );
    }

    // 19. classify_connectivity Unknown → Black
    #[test]
    fn classify_connectivity_unknown(_dummy in 0..1u8) {
        let obs = NetworkObserver::new();
        prop_assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Unknown),
            NetworkPressureTier::Black
        );
    }

    // 20. Observer with missing binary: attribute fails
    #[test]
    fn observer_missing_binary_attribute_fails(suffix in "[a-z]{3,10}") {
        let obs = NetworkObserver::with_binary(
            format!("/nonexistent-{}", suffix),
            NetworkObserverConfig::default(),
        );
        let result = obs.attribute_connection("10.0.0.1");
        prop_assert!(result.is_err());
    }

    // 21. Observer with missing binary: check_connectivity returns Unknown
    #[test]
    fn observer_missing_binary_connectivity_unknown(suffix in "[a-z]{3,10}") {
        let obs = NetworkObserver::with_binary(
            format!("/nonexistent-{}", suffix),
            NetworkObserverConfig::default(),
        );
        let status = obs.check_connectivity();
        prop_assert_eq!(status, ConnectivityStatus::Unknown);
    }

    // 22. attribute_failopen returns None for missing binary
    #[test]
    fn failopen_attribution_returns_none(suffix in "[a-z]{3,10}") {
        let obs = NetworkObserver::with_binary(
            format!("/nonexistent-{}", suffix),
            NetworkObserverConfig::default(),
        );
        let result = attribute_failopen(&obs, "10.0.0.1");
        prop_assert!(result.is_none());
    }

    // 23. pressure_failopen returns Green for missing binary
    #[test]
    fn failopen_pressure_returns_green(suffix in "[a-z]{3,10}") {
        let obs = NetworkObserver::with_binary(
            format!("/nonexistent-{}", suffix),
            NetworkObserverConfig::default(),
        );
        let tier = pressure_failopen(&obs, "10.0.0.1");
        prop_assert_eq!(tier, NetworkPressureTier::Green);
    }

    // 24. NetworkObserverError Display is non-empty
    #[test]
    fn error_display_non_empty(msg in "[a-z ]{1,20}") {
        let e = NetworkObserverError::BinaryNotFound(msg);
        prop_assert!(!e.to_string().is_empty());
    }

    // 25. NetworkObserverError::SubprocessFailed includes stderr
    #[test]
    fn error_subprocess_includes_stderr(stderr in "[a-z ]{1,20}") {
        let e = NetworkObserverError::SubprocessFailed {
            code: Some(1),
            stderr: stderr.clone(),
        };
        prop_assert!(e.to_string().contains(&stderr));
    }

    // 26. NetworkObserverError::ParseFailed includes message
    #[test]
    fn error_parse_includes_message(msg in "[a-z ]{1,20}") {
        let e = NetworkObserverError::ParseFailed(msg.clone());
        prop_assert!(e.to_string().contains(&msg));
    }

    // 27. Observer config() returns what was provided
    #[test]
    fn observer_config_matches(config in arb_config()) {
        let yellow = config.yellow_latency_ms;
        let red = config.red_latency_ms;
        let obs = NetworkObserver::with_config(config);
        prop_assert!((obs.config().yellow_latency_ms - yellow).abs() < 0.001);
        prop_assert!((obs.config().red_latency_ms - red).abs() < 0.001);
    }

    // 28. NetworkPressureTier display matches variant name
    #[test]
    fn pressure_tier_display_matches(tier in arb_pressure_tier()) {
        let display = tier.to_string();
        let expected = match tier {
            NetworkPressureTier::Green => "green",
            NetworkPressureTier::Yellow => "yellow",
            NetworkPressureTier::Red => "red",
            NetworkPressureTier::Black => "black",
        };
        prop_assert_eq!(display, expected);
    }

    // 29. ConnectivityStatus display matches variant name
    #[test]
    fn connectivity_display_matches(status in arb_connectivity_status()) {
        let display = status.to_string();
        let expected = match status {
            ConnectivityStatus::Connected => "connected",
            ConnectivityStatus::Degraded => "degraded",
            ConnectivityStatus::Unreachable => "unreachable",
            ConnectivityStatus::Unknown => "unknown",
        };
        prop_assert_eq!(display, expected);
    }

    // 30. Zero latency always classifies as Green
    #[test]
    fn zero_latency_always_green(config in arb_config()) {
        let obs = NetworkObserver::with_config(config);
        let attr = NetworkAttribution {
            provider: "t".into(),
            region: None,
            latency_ms: 0.0,
            is_trusted: false,
            remote_addr: "x".into(),
            asn: None,
            org: None,
        };
        prop_assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Green);
    }
}
