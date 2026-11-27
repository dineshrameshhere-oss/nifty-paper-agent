mod common;

use chrono::Duration;
use common::{
    bin_exe, buy_signal, clock_sleeper, ist_utc, load_repo_settings, make_quote,
    make_runner_deterministic, make_runner_with_backend, make_snapshot, repo_root,
    sample_expiries_json, sample_option_chain_json, test_settings,
    MockHttpResponse, MockIndstocksServer, ScriptedFeed, ScriptedLlmBackend,
};
use nifty_paper_agent::{
    clock::{market_hours_status, Clock, FakeClock, MarketHoursStatus},
    data::{IndstocksClient, MarketDataFeed},
    paper_runner::run_paper_session,
    risk,
    types::{Side, TraderProposal},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use std::{
    fs,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[test]
fn t3_f1_f2_docs_and_guardrail_alignment() {
    assert!(!LIVE_ORDER_PLACEMENT_IMPLEMENTED);
    let docs = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    let readme = fs::read_to_string(repo_root().join("README.md")).expect("read README.md");
    let lib_rs = fs::read_to_string(repo_root().join("src").join("lib.rs")).expect("read lib.rs");

    assert!(lib_rs.contains("pub const LIVE_ORDER_PLACEMENT_IMPLEMENTED: bool = false;"));
    assert!(docs.contains("LIVE_ORDER_PLACEMENT_IMPLEMENTED"));
    assert!(readme.contains("LIVE_ORDER_PLACEMENT_IMPLEMENTED = false"));
    assert!(docs.contains("https://api.indstocks.com"));
    assert!(readme.contains("https://api.indstocks.com"));
}

#[tokio::test]
async fn t3_f3_f4_settings_plus_raw_auth_client() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
        if req.path == "/market/instruments/expiries" {
            MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"]))
        } else {
            MockHttpResponse::ok(sample_option_chain_json(
                25350.0,
                "2026-12-03",
                "25350",
                "NIFTY-CE",
                100.0,
                101.0,
                100.5,
                "NIFTY-PE",
                90.0,
                91.0,
                90.5,
            ))
        }
    })
    .await;

    let mut settings = load_repo_settings();
    assert!(settings.indstocks.market_data_enabled);
    assert_eq!(settings.indstocks.option_chain_path, "/market/option-chain");
    settings.indstocks.rest_base_url = server.base_url().to_string();

    let token = "combo-raw-token-34";
    let client = IndstocksClient::new(settings.indstocks, token.to_string());
    let snap = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .expect("fetch_snapshot");
    assert_eq!(snap.quotes.len(), 2);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    for r in reqs {
        assert_eq!(
            r.headers.get("authorization").map(String::as_str),
            Some(token)
        );
    }
}

#[tokio::test]
async fn t3_f4_f5_auth_error_mid_chain_fetch() {
    // Expiries succeeds (200), option-chain fails with 401 TokenException
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::with_status(
            "401 Unauthorized",
            r#"{"status":"error","error_type":"TokenException","message":"Token expired mid-session"}"#,
        ),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "expiring-tok".to_string());
    let err = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 11, 30, 0))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("401"));
    assert!(msg.contains("TokenException"));
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn t3_f5_f6_indstocks_feed_plus_fake_clock_ist_freshness() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(sample_option_chain_json(
            25400.0,
            "2026-12-03",
            "25400",
            "NIFTY-25400-CE",
            115.0,
            116.0,
            115.5,
            "NIFTY-25400-PE",
            98.0,
            99.0,
            98.5,
        )),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    // Historical IST market open timestamp
    let sim_now = ist_utc(2025, 2, 10, 9, 15, 0);
    assert_eq!(market_hours_status(sim_now), MarketHoursStatus::Open);

    use nifty_paper_agent::data::MarketDataFeed;
    let snap = client.fetch_snapshot(sim_now).await.expect("fetch at sim_now");
    assert_eq!(snap.created_at, sim_now);
    assert!(risk::check_market_snapshot_at(&snap, 15, sim_now).is_ok());
    assert!(risk::check_market_snapshot_at(&snap, 15, sim_now + Duration::seconds(20)).is_err());
}

#[tokio::test]
async fn t3_f6_f7_fake_clock_plus_deterministic_tsl_exit() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal("f6f7-1", &q.symbol, 2, 101.0, 75.0, 91.0, 160.0)),
            }
        } else {
            TraderProposal {
                action: "HOLD".into(),
                rationale: "hold".into(),
                confidence_bps: 7000,
                signal: None,
            }
        }
    }));

    let (_db, _journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 30, 0));

    let s0 = make_snapshot(
        clock.now(),
        vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, clock.now())],
        "open",
    );
    runner.step_at(&s0, clock.now()).await.unwrap();

    clock.advance(Duration::seconds(5));
    let s1 = make_snapshot(
        clock.now(),
        vec![make_quote("NIFTY-CE", 125.0, 126.0, 125.0, clock.now())],
        "rally",
    );
    assert!(runner.monitor_positions_at(&s1, clock.now()).unwrap().is_empty());

    clock.advance(Duration::seconds(5));
    let s2 = make_snapshot(
        clock.now(),
        vec![make_quote("NIFTY-CE", 114.5, 115.5, 115.0, clock.now())],
        "tsl pullback",
    );
    let fills = runner.monitor_positions_at(&s2, clock.now()).unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].timestamp, clock.now());
    assert_eq!(runner.closed_trades()[0].exit_reason, "trailing_stop_loss");
}

#[tokio::test]
async fn t3_f7_f8_tick_exits_inside_continuous_paper_session() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 60;

    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, idx| {
        if pos.is_none() && idx == 0 {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    "f7f8-buy",
                    &q.symbol,
                    2,
                    102.0,
                    80.0,
                    92.0,
                    130.0,
                )),
            }
        } else {
            TraderProposal {
                action: "HOLD".into(),
                rationale: "hold".into(),
                confidence_bps: 7000,
                signal: None,
            }
        }
    }));
    let backend_ref = Arc::clone(&backend);

    let (_db, _journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 20, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        let px = match idx {
            0..=2 => 100.0,
            3 => 132.0, // hits target on tick 3
            _ => 100.0,
        };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", px, px + 0.5, px, now)],
            "f7f8",
        ))
    });

    let summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");

    assert_eq!(backend_ref.trader_call_count(), 1);
    assert_eq!(summary.total_trades, 1);
    assert_eq!(summary.closed_trades[0].exit_reason, "target_hit");
    assert_eq!(summary.eod_squared_off, 0);
}

#[tokio::test]
async fn t3_f8_f9_continuous_session_to_1530_eod_square_off() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 300;
    settings.analysis.bar_minutes = 30;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 0, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        let px = if market_hours_status(now) == MarketHoursStatus::Closed {
            116.0
        } else {
            100.0
        };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-25350-CE", px, px + 1.0, px + 0.5, now)],
            "hold to eod",
        ))
    });

    let summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");

    assert_eq!(market_hours_status(clock.now()), MarketHoursStatus::Closed);
    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.closed_trades[0].exit_reason, "eod_square_off");
    assert!(summary.realized_pnl_paise > 0);
}

#[tokio::test]
async fn t3_f2_f9_eod_square_off_never_touches_live_order_apis() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
        if req.path == "/market/instruments/expiries" {
            MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"]))
        } else {
            MockHttpResponse::ok(sample_option_chain_json(
                25350.0,
                "2026-12-03",
                "25350",
                "NIFTY-CE",
                110.0,
                111.0,
                110.5,
                "NIFTY-PE",
                95.0,
                96.0,
                95.5,
            ))
        }
    })
    .await;

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.poll_interval_seconds = 120;
    settings.analysis.bar_minutes = 60;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings.clone());
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 27, 0));

    let summary = run_paper_session(
        &mut runner,
        &client,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");

    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(server.non_get_rejections(), 0);
    assert!(server
        .requests()
        .iter()
        .all(|r| r.method == "GET" && r.path.starts_with("/market/")));
}

#[test]
fn t3_f3_f10_verified_settings_plus_cli_token_fail_fast() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Copy repo settings.toml verbatim (with market_data_enabled = true)
    let raw = fs::read_to_string(repo_root().join("settings.toml")).expect("read settings.toml");
    fs::write(dir.path().join("settings.toml"), raw).expect("write settings.toml");

    let output = Command::new(bin_exe())
        .current_dir(dir.path())
        .args(["paper", "--offline"])
        .env_remove("INDSTOCKS_ACCESS_TOKEN")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("spawn binary");

    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("INDSTOCKS_ACCESS_TOKEN"));
    assert!(combined.contains("scripts/update-indstocks-token.sh"));
}

#[tokio::test]
async fn t3_f5_f7_option_chain_snapshot_drives_runner_entry_and_tick_exit() {
    let chain_tick1 = sample_option_chain_json(
        25350.0,
        "2026-12-03",
        "25350",
        "NIFTY-25350-CE",
        100.0,
        101.0,
        100.5,
        "NIFTY-25350-PE",
        90.0,
        91.0,
        90.5,
    );
    let chain_tick2 = sample_option_chain_json(
        25450.0,
        "2026-12-03",
        "25350",
        "NIFTY-25350-CE",
        140.0,
        141.0,
        140.5,
        "NIFTY-25350-PE",
        70.0,
        71.0,
        70.5,
    );

    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(chain_tick1),
        MockHttpResponse::ok(chain_tick2),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks.clone(), "tok".to_string());
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    use nifty_paper_agent::data::MarketDataFeed;
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = client.fetch_snapshot(t0).await.expect("tick 1");
    let step = runner.step_at(&s0, t0).await.expect("entry step");
    assert!(step.fill.is_some());
    assert_eq!(runner.broker.open_positions_count(), 1);

    let t1 = t0 + Duration::seconds(5);
    let s1 = client.fetch_snapshot(t1).await.expect("tick 2");
    let exit_fills = runner.monitor_positions_at(&s1, t1).expect("tick exit");
    assert_eq!(exit_fills.len(), 1);
    assert_eq!(exit_fills[0].side, Side::Sell);
    assert_eq!(runner.broker.open_positions_count(), 0);
    assert_eq!(runner.closed_trades()[0].exit_reason, "target_hit");
}

#[tokio::test]
async fn t3_f6_f9_premarket_wait_through_eod_with_zero_trades() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 3600;
    settings.analysis.bar_minutes = 120;

    let backend = Arc::new(ScriptedLlmBackend::new(|_snap, _pos, _| TraderProposal {
        action: "HOLD".into(),
        rationale: "no setup".into(),
        confidence_bps: 5000,
        signal: None,
    }));

    let (_db, _journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 5, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now)],
            "flat day",
        ))
    });

    let summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");

    assert_eq!(summary.total_trades, 0);
    assert_eq!(summary.eod_squared_off, 0);
    assert_eq!(summary.starting_cash_paise, summary.ending_cash_paise);
    assert_eq!(summary.realized_pnl_paise, 0);
}

#[tokio::test]
async fn t3_f7_f9_partial_day_profit_plus_eod_square_off_pnl_reconciliation() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 5;

    let trade_seq = Arc::new(AtomicUsize::new(0));
    let ts = Arc::clone(&trade_seq);
    let backend = Arc::new(ScriptedLlmBackend::new(move |snap, pos, _| {
        if pos.is_none() {
            let n = ts.fetch_add(1, Ordering::SeqCst);
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    &format!("recon-{n}"),
                    &q.symbol,
                    2,
                    102.0,
                    80.0,
                    92.0,
                    130.0,
                )),
            }
        } else {
            TraderProposal {
                action: "HOLD".into(),
                rationale: "hold".into(),
                confidence_bps: 7000,
                signal: None,
            }
        }
    }));

    let (_db, _journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 20, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        // 15:20 -> buy #1 at 100; 15:22 -> target hit at 135; 15:25 -> buy #2 at 100; 15:30 -> EOD at 110
        let px = if idx == 2 {
            135.0
        } else if market_hours_status(now) == MarketHoursStatus::Closed {
            110.0
        } else {
            100.0
        };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", px, px + 0.5, px, now)],
            "reconcile",
        ))
    });

    let summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");

    assert_eq!(summary.total_trades, 2);
    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.open_positions_remaining, 0);
    let sum_trade_pnl: i64 = summary.closed_trades.iter().map(|t| t.realized_pnl_paise).sum();
    assert_eq!(sum_trade_pnl, summary.realized_pnl_paise);
    let cash_delta = summary.ending_cash_paise - summary.starting_cash_paise;
    assert!((cash_delta - summary.realized_pnl_paise).abs() <= 2);
}
