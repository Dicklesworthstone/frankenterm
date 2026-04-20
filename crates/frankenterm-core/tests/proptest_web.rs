//! Property-based tests for public `web` tuning/config carriers.

use frankenterm_core::tuning_config::WebTuning;
use frankenterm_core::web::{
    WebServerConfig, resolve_host, resolve_port, resolve_runtime_limits,
};
use proptest::prelude::*;

fn arb_host() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("127.0.0.1".to_string()),
        Just("localhost".to_string()),
        Just("::1".to_string()),
        "[a-z0-9.-]{1,24}".prop_map(|s| s),
    ]
}

fn arb_web_tuning() -> impl Strategy<Value = WebTuning> {
    (
        arb_host(),
        1u16..=u16::MAX,
        1usize..=10_000,
        1usize..=10_000,
        1usize..=1_000_000,
        1u32..=10_000,
        1u32..=10_000,
        1u64..=10_000,
        1usize..=10_000,
        1usize..=10_000,
    )
        .prop_filter(
            "default list limit must not exceed max list limit",
            |(_, _, max_list_limit, default_list_limit, _, _, _, _, _, _)| {
                default_list_limit <= max_list_limit
            },
        )
        .prop_filter(
            "default max hz must not exceed hard max hz",
            |(_, _, _, _, _, stream_default_max_hz, stream_max_max_hz, _, _, _)| {
                stream_default_max_hz <= stream_max_max_hz
            },
        )
        .prop_map(
            |(
                default_host,
                default_port,
                max_list_limit,
                default_list_limit,
                max_request_body_bytes,
                stream_default_max_hz,
                stream_max_max_hz,
                stream_keepalive_secs,
                stream_scan_limit,
                stream_scan_max_pages,
            )| WebTuning {
                default_host,
                default_port,
                max_list_limit,
                default_list_limit,
                max_request_body_bytes,
                stream_default_max_hz,
                stream_max_max_hz,
                stream_keepalive_secs,
                stream_scan_limit,
                stream_scan_max_pages,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn resolve_host_and_port_follow_tuning(tuning in arb_web_tuning()) {
        prop_assert_eq!(resolve_host(Some(&tuning)), tuning.default_host.clone());
        prop_assert_eq!(resolve_port(Some(&tuning)), tuning.default_port);
    }

    #[test]
    fn resolve_runtime_limits_follow_tuning(tuning in arb_web_tuning()) {
        let limits = resolve_runtime_limits(Some(&tuning));

        prop_assert_eq!(limits.max_list_limit, tuning.max_list_limit);
        prop_assert_eq!(limits.default_list_limit, tuning.default_list_limit);
        prop_assert_eq!(limits.max_request_body_bytes, tuning.max_request_body_bytes);
        prop_assert_eq!(limits.stream_default_max_hz, u64::from(tuning.stream_default_max_hz));
        prop_assert_eq!(limits.stream_max_max_hz, u64::from(tuning.stream_max_max_hz));
        prop_assert_eq!(limits.stream_keepalive_secs, tuning.stream_keepalive_secs);
        prop_assert_eq!(limits.stream_scan_limit, tuning.stream_scan_limit);
        prop_assert_eq!(limits.stream_scan_max_pages, tuning.stream_scan_max_pages);
    }

    #[test]
    fn web_server_config_builder_debug_tracks_overrides(
        host in arb_host(),
        port in 1u16..=u16::MAX,
        allow_public_bind in any::<bool>(),
    ) {
        let config = if allow_public_bind {
            WebServerConfig::new(0)
                .with_host(host.clone())
                .with_port(port)
                .with_dangerous_public_bind()
        } else {
            WebServerConfig::new(0).with_host(host.clone()).with_port(port)
        };

        let debug = format!("{config:?}");
        let allow_public_bind_text = format!("allow_public_bind: {}", allow_public_bind);
        prop_assert!(debug.contains("WebServerConfig"));
        prop_assert!(debug.contains(&host));
        prop_assert!(debug.contains(&port.to_string()));
        prop_assert!(debug.contains(&allow_public_bind_text));
        prop_assert!(debug.contains("storage: false"));
        prop_assert!(debug.contains("event_bus: false"));
    }
}
