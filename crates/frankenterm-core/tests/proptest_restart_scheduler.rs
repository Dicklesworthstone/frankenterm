//! Property tests for restart_scheduler serde roundtrips and scoring invariants.

use proptest::prelude::*;

use frankenterm_core::restart_scheduler::{
    ActivityProfile, RestartMode, RestartScheduler, RestartSchedulerConfig,
    RestartSchedulerTelemetry, ScheduledRestart, SchedulingDecision, ScoredWindow,
    activity_minimum, hazard_urgency, recency_penalty, score_window,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn finite_f64() -> impl Strategy<Value = f64> {
    prop::num::f64::NORMAL
        .prop_filter("finite", |x| x.is_finite())
        .prop_map(|x| x.clamp(-1e12, 1e12))
}

fn unit_f64() -> impl Strategy<Value = f64> {
    (0u32..=1000).prop_map(|n| n as f64 / 1000.0)
}

fn positive_f64() -> impl Strategy<Value = f64> {
    (1u32..=10_000).prop_map(|n| n as f64 / 100.0)
}

fn restart_mode_strategy() -> impl Strategy<Value = RestartMode> {
    prop_oneof![
        unit_f64().prop_map(|s| RestartMode::Automatic { min_score: s }),
        Just(RestartMode::Advisory),
        Just(RestartMode::Manual),
    ]
}

fn config_strategy() -> impl Strategy<Value = RestartSchedulerConfig> {
    (
        restart_mode_strategy(),
        unit_f64(),
        positive_f64(),
        1u32..120,
        any::<bool>(),
        unit_f64(),
        1u32..60,
        1u32..48,
    )
        .prop_map(
            |(mode, min_score, cooldown, warning, snapshot, threshold, window, lookahead)| {
                RestartSchedulerConfig {
                    mode,
                    min_score_threshold: min_score,
                    cooldown_hours: cooldown,
                    advance_warning_minutes: warning,
                    pre_restart_snapshot: snapshot,
                    hazard_threshold: threshold,
                    window_minutes: window,
                    lookahead_hours: lookahead,
                }
            },
        )
}

fn activity_profile_strategy() -> impl Strategy<Value = ActivityProfile> {
    (unit_f64(), prop::collection::vec(unit_f64(), 24..=24)).prop_map(|(alpha, activities)| {
        let mut profile = ActivityProfile::new(alpha);
        for (h, &a) in activities.iter().enumerate() {
            profile.update(h as u32, a);
        }
        profile
    })
}

fn scored_window_strategy() -> impl Strategy<Value = ScoredWindow> {
    (
        0u32..1440,
        0u32..24,
        unit_f64(),
        unit_f64(),
        unit_f64(),
        unit_f64(),
    )
        .prop_map(|(offset, hour, hu, am, rp, score)| ScoredWindow {
            offset_minutes: offset,
            hour_of_day: hour,
            hazard_urgency: hu,
            activity_minimum: am,
            recency_penalty: rp,
            score,
        })
}

fn scheduled_restart_strategy() -> impl Strategy<Value = ScheduledRestart> {
    (
        1_600_000_000_000i64..1_800_000_000_000i64,
        unit_f64(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(ts, score, notified, snapshot)| ScheduledRestart {
            scheduled_at_ms: ts,
            score,
            notified,
            snapshot_taken: snapshot,
        })
}

fn scheduling_decision_strategy() -> impl Strategy<Value = SchedulingDecision> {
    (
        prop::collection::vec(scored_window_strategy(), 0..10),
        prop::option::of(scored_window_strategy()),
        any::<bool>(),
    )
        .prop_map(|(windows, rec, trigger)| SchedulingDecision {
            windows,
            recommendation: rec,
            would_trigger: trigger,
        })
}

fn telemetry_strategy() -> impl Strategy<Value = RestartSchedulerTelemetry> {
    (
        prop::sample::select(vec![
            "Advisory".to_string(),
            "Manual".to_string(),
            "Automatic { min_score: 0.7 }".to_string(),
        ]),
        any::<u64>(),
        prop::option::of(1_600_000_000_000i64..1_800_000_000_000i64),
        prop::option::of(1_600_000_000_000i64..1_800_000_000_000i64),
        prop::option::of(unit_f64()),
        0u32..24,
    )
        .prop_map(
            |(mode, obs, last, sched, score, hour)| RestartSchedulerTelemetry {
                mode,
                observations: obs,
                last_restart_ms: last,
                scheduled_at_ms: sched,
                scheduled_score: score,
                min_activity_hour: hour,
            },
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn f64_close(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    #[allow(clippy::float_cmp)]
    if a == b {
        return true;
    }
    let abs_diff = (a - b).abs();
    let max_abs = a.abs().max(b.abs());
    if max_abs > 1.0 {
        abs_diff / max_abs < 1e-10
    } else {
        abs_diff < 1e-10
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn restart_mode_serde_roundtrip(mode in restart_mode_strategy()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: RestartMode = serde_json::from_str(&json).unwrap();
        match (&mode, &back) {
            (RestartMode::Automatic { min_score: a }, RestartMode::Automatic { min_score: b }) => {
                let close = f64_close(*a, *b);
                prop_assert!(close, "Automatic min_score mismatch: {} vs {}", a, b);
            }
            (RestartMode::Advisory, RestartMode::Advisory) => {}
            (RestartMode::Manual, RestartMode::Manual) => {}
            _ => prop_assert!(false, "mode variant mismatch: {:?} vs {:?}", mode, back),
        }
    }

    #[test]
    fn config_serde_roundtrip(config in config_strategy()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RestartSchedulerConfig = serde_json::from_str(&json).unwrap();
        let close_cooldown = f64_close(config.cooldown_hours, back.cooldown_hours);
        prop_assert!(close_cooldown, "cooldown mismatch");
        let close_threshold = f64_close(config.min_score_threshold, back.min_score_threshold);
        prop_assert!(close_threshold, "threshold mismatch");
        prop_assert_eq!(config.advance_warning_minutes, back.advance_warning_minutes);
        prop_assert_eq!(config.pre_restart_snapshot, back.pre_restart_snapshot);
        prop_assert_eq!(config.window_minutes, back.window_minutes);
        prop_assert_eq!(config.lookahead_hours, back.lookahead_hours);
    }

    #[test]
    fn activity_profile_serde_roundtrip(profile in activity_profile_strategy()) {
        let json = serde_json::to_string(&profile).unwrap();
        let back: ActivityProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(profile.observations(), back.observations());
        for h in 0..24 {
            let close = f64_close(profile.predict(h), back.predict(h));
            prop_assert!(close, "hour {} mismatch: {} vs {}", h, profile.predict(h), back.predict(h));
        }
    }

    #[test]
    fn scored_window_serde_roundtrip(sw in scored_window_strategy()) {
        let json = serde_json::to_string(&sw).unwrap();
        let back: ScoredWindow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sw.offset_minutes, back.offset_minutes);
        prop_assert_eq!(sw.hour_of_day, back.hour_of_day);
        let close = f64_close(sw.score, back.score);
        prop_assert!(close, "score mismatch: {} vs {}", sw.score, back.score);
    }

    #[test]
    fn scheduled_restart_serde_roundtrip(sr in scheduled_restart_strategy()) {
        let json = serde_json::to_string(&sr).unwrap();
        let back: ScheduledRestart = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sr.scheduled_at_ms, back.scheduled_at_ms);
        prop_assert_eq!(sr.notified, back.notified);
        prop_assert_eq!(sr.snapshot_taken, back.snapshot_taken);
        let close = f64_close(sr.score, back.score);
        prop_assert!(close, "score mismatch");
    }

    #[test]
    fn scheduling_decision_serde_roundtrip(sd in scheduling_decision_strategy()) {
        let json = serde_json::to_string(&sd).unwrap();
        let back: SchedulingDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sd.windows.len(), back.windows.len());
        prop_assert_eq!(sd.would_trigger, back.would_trigger);
        let rec_match = sd.recommendation.is_some() == back.recommendation.is_some();
        prop_assert!(rec_match, "recommendation presence mismatch");
    }

    #[test]
    fn telemetry_serde_roundtrip(tel in telemetry_strategy()) {
        let json = serde_json::to_string(&tel).unwrap();
        let back: RestartSchedulerTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&tel.mode, &back.mode);
        prop_assert_eq!(tel.observations, back.observations);
        prop_assert_eq!(tel.last_restart_ms, back.last_restart_ms);
        prop_assert_eq!(tel.scheduled_at_ms, back.scheduled_at_ms);
        prop_assert_eq!(tel.min_activity_hour, back.min_activity_hour);
    }

    #[test]
    fn scheduler_serde_roundtrip(config in config_strategy()) {
        let mut scheduler = RestartScheduler::new(config);
        scheduler.update_activity(3, 0.2);
        scheduler.update_activity(15, 0.8);
        let json = serde_json::to_string(&scheduler).unwrap();
        let back: RestartScheduler = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            scheduler.activity_profile().observations(),
            back.activity_profile().observations()
        );
    }
}

// ---------------------------------------------------------------------------
// Scoring invariant tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn hazard_urgency_bounded(rate in unit_f64(), threshold in unit_f64()) {
        let u = hazard_urgency(rate, threshold);
        prop_assert!((0.0..=1.0).contains(&u), "urgency out of bounds: {}", u);
    }

    #[test]
    fn hazard_urgency_monotone(threshold in unit_f64()) {
        // Higher hazard rate → higher urgency (monotonically increasing)
        let u_low = hazard_urgency(0.1, threshold);
        let u_high = hazard_urgency(0.9, threshold);
        prop_assert!(u_high >= u_low, "urgency not monotone: low={}, high={}", u_low, u_high);
    }

    #[test]
    fn activity_minimum_bounded(activity in finite_f64()) {
        let am = activity_minimum(activity);
        prop_assert!((0.0..=1.0).contains(&am), "activity_minimum out of bounds: {}", am);
    }

    #[test]
    fn recency_penalty_bounded(hours in positive_f64(), cooldown in positive_f64()) {
        let p = recency_penalty(hours, cooldown);
        prop_assert!((0.0..=1.0).contains(&p), "recency_penalty out of bounds: {}", p);
    }

    #[test]
    fn recency_penalty_monotone(cooldown in positive_f64()) {
        // More time since last restart → higher penalty (less suppression)
        let p_recent = recency_penalty(1.0, cooldown);
        let p_old = recency_penalty(100.0, cooldown);
        prop_assert!(p_old >= p_recent, "penalty not monotone: recent={}, old={}", p_recent, p_old);
    }

    #[test]
    fn score_window_non_negative(
        hazard in unit_f64(),
        activity in unit_f64(),
        hours in positive_f64(),
    ) {
        let config = RestartSchedulerConfig::default();
        let s = score_window(hazard, activity, hours, &config);
        prop_assert!(s >= 0.0, "score negative: {}", s);
    }

    #[test]
    fn evaluate_windows_sorted_descending(
        config in config_strategy(),
        current_hour in 0u32..24,
        num_rates in 1usize..50,
    ) {
        let scheduler = RestartScheduler::new(config);
        let hazard_rates: Vec<f64> = (0..num_rates)
            .map(|i| (i as f64) / (num_rates as f64))
            .collect();
        let decision = scheduler.evaluate(1_700_000_000_000, current_hour, &hazard_rates);
        for pair in decision.windows.windows(2) {
            prop_assert!(
                pair[0].score >= pair[1].score || (pair[0].score.is_nan() && pair[1].score.is_nan()),
                "windows not sorted: {} > {}",
                pair[0].score,
                pair[1].score
            );
        }
    }

    #[test]
    fn manual_mode_never_triggers(
        num_rates in 1usize..20,
        current_hour in 0u32..24,
    ) {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Manual,
            ..Default::default()
        };
        let scheduler = RestartScheduler::new(config);
        let hazard_rates: Vec<f64> = vec![1.0; num_rates];
        let decision = scheduler.evaluate(1_700_000_000_000, current_hour, &hazard_rates);
        prop_assert!(!decision.would_trigger, "Manual mode should never trigger");
    }

    #[test]
    fn advisory_mode_never_triggers(
        num_rates in 1usize..20,
        current_hour in 0u32..24,
    ) {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Advisory,
            min_score_threshold: 0.0,
            ..Default::default()
        };
        let scheduler = RestartScheduler::new(config);
        let hazard_rates: Vec<f64> = vec![1.0; num_rates];
        let decision = scheduler.evaluate(1_700_000_000_000, current_hour, &hazard_rates);
        prop_assert!(!decision.would_trigger, "Advisory mode should never trigger");
    }

    #[test]
    fn activity_profile_min_hour_in_range(profile in activity_profile_strategy()) {
        let min_h = profile.min_activity_hour();
        prop_assert!(min_h < 24, "min_activity_hour out of range: {}", min_h);
    }

    #[test]
    fn activity_profile_predict_stable(profile in activity_profile_strategy(), hour in 0u32..24) {
        let a = profile.predict(hour);
        let b = profile.predict(hour);
        let close = f64_close(a, b);
        prop_assert!(close, "predict not stable: {} vs {}", a, b);
    }
}
