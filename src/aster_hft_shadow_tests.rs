use super::aster_hft_shadow::{HftLabConfig, HftProfileConfig, HftShadowLab};

fn lab() -> HftShadowLab {
    HftShadowLab::new(HftLabConfig {
        tick_size: 0.1,
        quote_notional: 20.0,
        penetration_bps: 0.2,
        fill_ratio: 0.5,
        toxicity_1s_bps: -2.0,
        toxicity_min_samples: 8,
        profiles: vec![
            HftProfileConfig {
                name: "join-250ms".into(),
                offset_bps: 0.0,
                requote_threshold_ticks: 1,
                cooldown_ms: 250,
            },
            HftProfileConfig {
                name: "offset-1bp".into(),
                offset_bps: 1.0,
                requote_threshold_ticks: 1,
                cooldown_ms: 1_000,
            },
        ],
    })
    .unwrap()
}

#[test]
fn profiles_place_tick_aligned_two_sided_quotes() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    let snapshot = lab.snapshot(1_000);
    assert_eq!(snapshot.profiles.len(), 2);

    let join = &snapshot.profiles[0];
    assert_eq!(join.name, "join-250ms");
    assert_eq!(join.buy_price, Some(100.0));
    assert!((join.sell_price.unwrap() - 100.1).abs() < 1e-9);
    assert_eq!(join.metrics.active_quotes, 2);
    assert_eq!(join.metrics.estimated_order_requests, 2);

    let offset = &snapshot.profiles[1];
    assert!((offset.buy_price.unwrap() - 99.9).abs() < 1e-9);
    assert!((offset.sell_price.unwrap() - 100.2).abs() < 1e-9);
}

#[test]
fn unchanged_bbo_does_not_create_quote_churn() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_100, 1_100);
    let snapshot = lab.snapshot(1_100);
    for profile in snapshot.profiles {
        assert_eq!(profile.metrics.quote_requotes, 0);
        assert_eq!(profile.metrics.estimated_order_requests, 2);
    }
}

#[test]
fn requote_respects_cooldown_and_amends_in_place() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    lab.observe_bbo("BTCUSDT", 100.2, 100.3, 1_100, 1_100);
    assert_eq!(lab.snapshot(1_100).profiles[0].metrics.active_quotes, 2);
    assert_eq!(lab.snapshot(1_100).profiles[0].metrics.quote_requotes, 0);

    lab.observe_bbo("BTCUSDT", 100.2, 100.3, 1_300, 1_300);
    let amend_phase = lab.snapshot(1_300);
    assert_eq!(amend_phase.profiles[0].metrics.active_quotes, 2);
    assert_eq!(amend_phase.profiles[0].metrics.quote_requotes, 2);
    assert_eq!(amend_phase.profiles[0].metrics.estimated_order_requests, 6);
    assert_eq!(amend_phase.profiles[0].metrics.estimated_modify_requests, 4);
    assert_eq!(amend_phase.profiles[0].metrics.modify_request_savings, 2);

    lab.observe_bbo("BTCUSDT", 100.2, 100.3, 1_400, 1_400);
    let steady = lab.snapshot(1_400);
    assert_eq!(steady.profiles[0].metrics.active_quotes, 2);
    assert_eq!(steady.profiles[0].metrics.estimated_order_requests, 6);
}

#[test]
fn ranking_prefers_the_profile_with_fills_and_better_markout() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    assert_eq!(lab.snapshot(1_000).recommended_profile, None);

    lab.observe_bbo("BTCUSDT", 99.8, 99.9, 1_300, 1_300);
    let ranked = lab.snapshot(1_300);
    assert_eq!(ranked.recommended_profile.as_deref(), Some("join-250ms"));
    assert!(ranked.recommendation_reason.contains("join-250ms"));
    assert!(ranked.profiles[0].metrics.virtual_fills > 0);
    assert_eq!(ranked.profiles[1].metrics.virtual_fills, 0);
}

#[test]
fn depth_and_fill_metrics_remain_isolated_per_profile() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    lab.observe_depth(
        "BTCUSDT",
        &[(100.0, 10.0), (99.9, 20.0)],
        &[(100.1, 12.0), (100.2, 18.0)],
        1_050,
        1_050,
    );
    lab.observe_bbo("BTCUSDT", 99.8, 99.9, 1_300, 1_300);
    let snapshot = lab.snapshot(1_300);
    assert!(snapshot.profiles[0].metrics.visible_queue_samples > 0);
    assert!(snapshot.profiles[0].metrics.virtual_fills > 0);
    assert_eq!(snapshot.profiles[1].metrics.virtual_fills, 0);
}

#[test]
fn suspending_lab_clears_all_profiles_and_freezes_metrics() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    lab.set_collecting(false, 2_000);
    lab.observe_bbo("BTCUSDT", 99.0, 99.1, 3_000, 3_000);
    let snapshot = lab.snapshot(5_000);
    assert!(!snapshot.collecting);
    for profile in snapshot.profiles {
        assert_eq!(profile.metrics.active_quotes, 0);
        assert_eq!(profile.metrics.virtual_fills, 0);
        assert_eq!(profile.metrics.runtime_seconds, 1.0);
    }
}

#[test]
fn crossed_or_locked_book_pulls_virtual_quotes() {
    let mut lab = lab();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    assert_eq!(lab.snapshot(1_000).profiles[0].metrics.active_quotes, 2);

    lab.observe_bbo("BTCUSDT", 100.2, 100.0, 1_100, 1_100);
    let crossed = lab.snapshot(1_100);
    for profile in crossed.profiles {
        assert_eq!(profile.metrics.active_quotes, 0);
        assert!(profile.buy_price.is_none());
        assert!(profile.sell_price.is_none());
    }
}

#[test]
fn toxic_one_second_markout_latches_and_is_not_recommended() {
    let mut lab = HftShadowLab::new(HftLabConfig {
        tick_size: 0.1,
        quote_notional: 20.0,
        penetration_bps: 0.2,
        fill_ratio: 0.5,
        toxicity_1s_bps: -1.0,
        toxicity_min_samples: 1,
        profiles: vec![HftProfileConfig {
            name: "join-250ms".into(),
            offset_bps: 0.0,
            requote_threshold_ticks: 1,
            cooldown_ms: 250,
        }],
    })
    .unwrap();
    lab.observe_bbo("BTCUSDT", 100.0, 100.1, 1_000, 1_000);
    lab.observe_bbo("BTCUSDT", 99.8, 99.9, 1_100, 1_100);
    lab.observe_bbo("BTCUSDT", 98.9, 99.0, 2_100, 2_100);
    let snapshot = lab.snapshot(2_100);
    assert!(snapshot.profiles[0].toxic);
    assert_eq!(snapshot.profiles[0].metrics.active_quotes, 0);
    assert_eq!(snapshot.recommended_profile, None);
}

#[test]
fn toxicity_config_must_fail_closed() {
    let mut config = HftLabConfig {
        tick_size: 0.1,
        quote_notional: 20.0,
        penetration_bps: 0.2,
        fill_ratio: 0.5,
        toxicity_1s_bps: 1.0,
        toxicity_min_samples: 8,
        profiles: vec![HftProfileConfig {
            name: "join".into(),
            offset_bps: 0.0,
            requote_threshold_ticks: 1,
            cooldown_ms: 250,
        }],
    };
    assert!(HftShadowLab::new(config.clone()).is_err());
    config.toxicity_1s_bps = -1.0;
    config.toxicity_min_samples = 0;
    assert!(HftShadowLab::new(config).is_err());
}
