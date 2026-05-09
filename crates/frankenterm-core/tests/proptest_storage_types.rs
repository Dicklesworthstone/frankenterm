use proptest::prelude::*;
use std::str::FromStr;

use frankenterm_core::storage::{
    ApprovalTokenRecord, CorrelationType, DatabasePageStats, MetricType, NotificationStatus,
    PaneReservation, PaneReservationConfig, TimelineQuery,
};

fn metric_type_strategy() -> impl Strategy<Value = MetricType> {
    prop::sample::select(vec![
        MetricType::TokenUsage,
        MetricType::ApiCost,
        MetricType::ApiCall,
        MetricType::RateLimitHit,
        MetricType::WorkflowCost,
        MetricType::SessionDuration,
    ])
}

fn notification_status_strategy() -> impl Strategy<Value = NotificationStatus> {
    prop::sample::select(vec![
        NotificationStatus::Pending,
        NotificationStatus::Sent,
        NotificationStatus::Failed,
        NotificationStatus::Throttled,
    ])
}

fn correlation_type_strategy() -> impl Strategy<Value = CorrelationType> {
    prop::sample::select(vec![
        CorrelationType::Failover,
        CorrelationType::Cascade,
        CorrelationType::Temporal,
        CorrelationType::WorkflowGroup,
        CorrelationType::DedupeGroup,
    ])
}

fn short_label() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,15}"
}

fn noncanonical_label() -> impl Strategy<Value = String> {
    prop_oneof![
        "[A-Z][A-Za-z0-9_]{0,15}",
        "[a-z][a-z0-9_]{0,8}-bad",
        "[a-z][a-z0-9_]{0,8} bad",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_storage_types_free_ratio_is_zero_or_clamped_to_unit_interval(
        page_count in -1_000_i64..=1_000_000,
        free_pages in -1_000_i64..=1_500_000,
    ) {
        let stats = DatabasePageStats {
            page_count,
            free_pages,
        };

        let ratio = stats.free_ratio();

        prop_assert!((0.0..=1.0).contains(&ratio));
        if page_count <= 0 || free_pages <= 0 {
            prop_assert!(ratio.abs() <= f64::EPSILON);
        } else if free_pages >= page_count {
            prop_assert!((ratio - 1.0).abs() <= f64::EPSILON);
        } else {
            let expected = free_pages as f64 / page_count as f64;
            prop_assert!((ratio - expected).abs() <= f64::EPSILON);
        }
    }

    #[test]
    fn proptest_storage_types_timeline_query_builders_preserve_inputs(
        start in any::<i64>(),
        end in any::<i64>(),
        panes in prop::collection::vec(any::<u64>(), 0..=8),
        severities in prop::collection::vec(short_label(), 0..=6),
        limit in any::<usize>(),
        offset in any::<usize>(),
    ) {
        let query = TimelineQuery::new()
            .with_range(start, end)
            .with_panes(panes.clone())
            .with_severities(severities.clone())
            .unhandled_only()
            .with_pagination(limit, offset);

        prop_assert_eq!(query.start, Some(start));
        prop_assert_eq!(query.end, Some(end));
        prop_assert_eq!(query.pane_ids, Some(panes));
        prop_assert_eq!(query.severities, Some(severities));
        prop_assert!(query.unhandled_only);
        prop_assert!(query.include_correlations);
        prop_assert_eq!(query.limit, limit);
        prop_assert_eq!(query.offset, offset);
    }

    #[test]
    fn proptest_storage_types_string_codecs_roundtrip_known_variants(
        metric_type in metric_type_strategy(),
        notification_status in notification_status_strategy(),
        correlation_type in correlation_type_strategy(),
    ) {
        prop_assert_eq!(metric_type.as_str(), metric_type.to_string());
        prop_assert_eq!(MetricType::from_str(metric_type.as_str()).unwrap(), metric_type);

        prop_assert_eq!(notification_status.as_str(), notification_status.to_string());
        prop_assert_eq!(
            NotificationStatus::from_str(notification_status.as_str()).unwrap(),
            notification_status,
        );

        let correlation_label = correlation_type.to_string();
        prop_assert!(!correlation_label.is_empty());
        prop_assert!(correlation_label
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '_'));
    }

    #[test]
    fn proptest_storage_types_string_codecs_reject_unknown_labels(
        label in noncanonical_label(),
    ) {
        prop_assert!(MetricType::from_str(&label).is_err());
        prop_assert!(NotificationStatus::from_str(&label).is_err());
    }

    #[test]
    fn proptest_storage_types_approval_token_active_matches_unused_unexpired_rule(
        now_ms in -10_000_i64..=10_000,
        expires_at in -10_000_i64..=10_000,
        used_at in prop::option::of(-10_000_i64..=10_000),
    ) {
        let token = ApprovalTokenRecord {
            id: 1,
            code_hash: "hash".to_string(),
            created_at: 0,
            expires_at,
            used_at,
            workspace_id: "workspace".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(42),
            action_fingerprint: "fingerprint".to_string(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };

        prop_assert_eq!(token.is_active(now_ms), used_at.is_none() && expires_at >= now_ms);
    }

    #[test]
    fn proptest_storage_types_pane_reservation_active_and_ttl_clamp_are_exact(
        now_ms in -10_000_i64..=10_000,
        expires_at in -10_000_i64..=10_000,
        released_at in prop::option::of(-10_000_i64..=10_000),
        active_label in any::<bool>(),
        requested_ttl_ms in -10_000_i64..=20_000_000,
        max_ttl_ms in 1_000_i64..=20_000_000,
    ) {
        let status = if active_label { "active" } else { "released" }.to_string();
        let reservation = PaneReservation {
            id: 1,
            pane_id: 42,
            owner_kind: "workflow".to_string(),
            owner_id: "workflow-1".to_string(),
            reason: None,
            created_at: 0,
            expires_at,
            released_at,
            status: status.clone(),
        };
        let config = PaneReservationConfig {
            default_ttl_ms: 1_000,
            max_ttl_ms,
        };

        prop_assert_eq!(
            reservation.is_active(now_ms),
            status == "active" && released_at.is_none() && expires_at > now_ms,
        );
        prop_assert_eq!(
            config.clamp_ttl(requested_ttl_ms),
            requested_ttl_ms.clamp(1_000, max_ttl_ms),
        );
    }
}
