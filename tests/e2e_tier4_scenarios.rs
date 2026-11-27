mod common;

use chrono::{Duration, Timelike};
use common::{
    bin_exe, buy_signal, clock_sleeper, ist_utc, load_repo_settings, make_quote,
    make_runner_deterministic, make_runner_with_backend, make_snapshot, sample_expiries_json,
    sample_option_chain_json, temp_journal, test_settings, write_isolated_settings,
    MockHttpResponse, MockIndstocksServer, ScriptedFeed, ScriptedLlmBackend,
};
use nifty_paper_agent::{
    agents::TradingAgentsOrchestrator,
    clock::{market_hours_status, to_ist, Clock, FakeClock, MarketHoursStatus},
    data::IndstocksClient,
    paper_runner::{run_paper_session, PaperRunner},
    types::TraderProposal,
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use std::{
    collections::BTreeMap,
    fs,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

#[tokio::test]
async fn t4_scenario_1_full_nse_day_win_with_tsl_ratchet_and_eod_square_off() {
    assert!(!LIVE_ORDER_PLACEMENT_IMPLEMENTED);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 10, 0));
    let clk_srv = clock.clone();

    let server = MockIndstocksServer::start_with_handler(true, move |req| {
        assert_eq!(
            req.headers.get("authorization").map(String::as_str),
            Some("scenario1-daily-token")
        );
        if req.path == "/market/instruments/expiries" {
            return MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"]));
        }
        let ist = to_ist(clk_srv.now());
        let mins = ist.hour() * 60 + ist.minute();
        // Morning rally 09:15..10:15 (100 -> 130), pullback at 10:30..12:00 (118), afternoon steady 105 -> EOD 118
        let (ce_bid, ce_ask, ce_last) = if mins < 9 * 60 + 30 {
            (100.0, 100.5, 100.2)
        } else if mins < 10 * 60 + 30 {
            (129.0, 131.0, 130.0)
        } else if mins < 12 * 60 + 15
            || market_hours_status(clk_srv.now()) == MarketHoursStatus::Closed
        {
            (118.0, 119.0, 118.5)
        } else {
            (105.0, 105.5, 105.2)
        };
        MockHttpResponse::ok(sample_option_chain_json(
            25350.0,
            "2026-12-03",
            "25350",
            "NIFTY-2026-12-03-25350-CE",
            ce_bid,
            ce_ask,
            ce_last,
            "NIFTY-2026-12-03-25350-PE",
            90.0,
            91.0,
            90.5,
        ))
    })
    .await;

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.poll_interval_seconds = 900; // 15-min ticks
    settings.analysis.bar_minutes = 180; // Morning 09:15 + Afternoon 12:15

    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, idx| {
        if pos.is_none() {
            let q = snap
                .quotes
                .iter()
                .find(|q| q.symbol.ends_with("-CE"))
                .unwrap();
            // entry=108, tsl=98 -> trailing_distance = 10
            TraderProposal {
                action: "BUY".into(),
                rationale: "trend continuation".into(),
                confidence_bps: 8200,
                signal: Some(buy_signal(
                    &format!("sc1-entry-{idx}"),
                    &q.symbol,
                    2,
                    108.0,
                    75.0,
                    98.0,
                    160.0,
                )),
            }
        } else {
            TraderProposal {
                action: "HOLD".into(),
                rationale: "let runner trail".into(),
                confidence_bps: 7500,
                signal: None,
            }
        }
    }));

    let (_db, journal, mut runner) = make_runner_with_backend(settings.clone(), backend);
    let client = IndstocksClient::new(settings.indstocks, "scenario1-daily-token".to_string());

    let summary = run_paper_session(
        &mut runner,
        &client,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("full day session completes");

    assert_eq!(summary.date_ist, "2026-11-27");
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.total_trades, 2);
    assert_eq!(summary.eod_squared_off, 1);
    assert!(summary
        .closed_trades
        .iter()
        .any(|t| t.exit_reason == "trailing_stop_loss"));
    assert!(summary
        .closed_trades
        .iter()
        .any(|t| t.exit_reason == "eod_square_off"));
    assert!(summary.realized_pnl_paise > 0);
    assert_eq!(journal.reflection_count().unwrap(), 2);
    assert_eq!(server.non_get_rejections(), 0);
    let report = summary.format_summary();
    assert!(report.contains("2026-11-27 IST"));
    assert!(report.contains("trailing_stop_loss"));
    assert!(report.contains("eod_square_off"));
}

#[tokio::test]
async fn t4_scenario_2_losing_sl_day_and_daily_loss_cap_protection() {
    let mut settings = test_settings(None);
    settings.paper.starting_capital_paise = 1_000_000;
    settings.paper.max_daily_loss_paise = 8_000; // ₹80 daily loss cap
    settings.indstocks.poll_interval_seconds = 1800;
    settings.analysis.bar_minutes = 60;

    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, idx| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "whipsaw attempt".into(),
                confidence_bps: 7500,
                signal: Some(buy_signal(
                    &format!("sc2-{idx}"),
                    &q.symbol,
                    5,
                    102.0,
                    80.0,
                    90.0,
                    140.0,
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

    let (_db, journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 15, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        // Tick 0 enters at ~100, Tick 1 crashes to 80 (loss ~₹100 > ₹80 cap), remaining ticks recover to 100
        let px = if idx == 1 { 80.0 } else { 100.0 };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-25350-PE", px, px + 0.5, px, now)],
            "volatile whipsaw",
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

    assert_eq!(summary.total_trades, 1, "daily loss cap must block re-entry");
    assert_eq!(summary.closed_trades[0].exit_reason, "stop_loss");
    assert!(summary.realized_pnl_paise < 0);
    assert!(runner.daily_loss_paise() >= 8_000);
    // Remaining capital protected above ₹9,850 (started ₹10,000, lost ~₹100)
    assert!(summary.ending_cash_paise > 985_000);
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(journal.reflection_count().unwrap(), 1);
}

#[tokio::test]
async fn t4_scenario_3_multi_position_day_and_eod_batch_square_off() {
    let mut settings = test_settings(None);
    settings.paper.max_open_positions = 3;
    settings.indstocks.poll_interval_seconds = 900; // 15m
    settings.analysis.bar_minutes = 60; // 1h bars

    let backend = Arc::new(ScriptedLlmBackend::new(|_snap, _pos, idx| {
        let (sym, target) = match idx {
            0 => ("NIFTY-STRIKE-1", 125.0),
            1 => ("NIFTY-STRIKE-2", 180.0),
            2 => ("NIFTY-STRIKE-3", 180.0),
            3 => ("NIFTY-STRIKE-4", 180.0),
            _ => ("NIFTY-STRIKE-5", 180.0), // blocked by max_open_positions=3
        };
        TraderProposal {
            action: "BUY".into(),
            rationale: "multi-leg day".into(),
            confidence_bps: 8000,
            signal: Some(buy_signal(
                &format!("sc3-{idx}"),
                sym,
                1,
                102.0,
                80.0,
                92.0,
                target,
            )),
        }
    }));

    let (_db, _journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 15, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        let s1_px = if idx == 1 { 126.0 } else { 100.0 };
        Ok(make_snapshot(
            now,
            vec![
                make_quote("NIFTY-STRIKE-1", s1_px, s1_px + 0.5, s1_px, now),
                make_quote("NIFTY-STRIKE-2", 100.0, 100.5, 100.2, now),
                make_quote("NIFTY-STRIKE-3", 100.0, 100.5, 100.2, now),
                make_quote("NIFTY-STRIKE-4", 100.0, 100.5, 100.2, now),
                make_quote("NIFTY-STRIKE-5", 100.0, 100.5, 100.2, now),
            ],
            "expiry multi-strike chain",
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

    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.eod_squared_off, 3);
    assert_eq!(summary.total_trades, 4); // 1 target_hit + 3 eod_square_off
    assert!(summary
        .closed_trades
        .iter()
        .any(|t| t.symbol == "NIFTY-STRIKE-1" && t.exit_reason == "target_hit"));
    for sym in ["NIFTY-STRIKE-2", "NIFTY-STRIKE-3", "NIFTY-STRIKE-4"] {
        assert!(summary
            .closed_trades
            .iter()
            .any(|t| t.symbol == sym && t.exit_reason == "eod_square_off"));
    }
}

#[tokio::test]
async fn t4_scenario_4_two_day_paper_run_with_token_rotation_and_memory_recall() {
    let dir = tempfile::tempdir().expect("isolated dir");
    let env_path = dir.path().join(".env");

    // Day 1 .env setup (UTF-8 without BOM)
    fs::write(
        &env_path,
        "GEMINI_API_KEY=keep-gemini-secret\nINDSTOCKS_ACCESS_TOKEN=day1-token\n",
    )
    .expect("write day1 .env");

    let expected_token = Arc::new(Mutex::new("day1-token".to_string()));
    let exp_tok_ref = Arc::clone(&expected_token);
    let server = MockIndstocksServer::start_with_handler(true, move |req| {
        let want = exp_tok_ref.lock().unwrap().clone();
        if req.headers.get("authorization").map(String::as_str) != Some(want.as_str()) {
            return MockHttpResponse::with_status(
                "401 Unauthorized",
                r#"{"status":"error","error_type":"TokenException","message":"Expired token"}"#,
            );
        }
        if req.path == "/market/instruments/expiries" {
            MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"]))
        } else {
            MockHttpResponse::ok(sample_option_chain_json(
                25350.0,
                "2026-12-03",
                "25350",
                "NIFTY-2026-12-03-25350-CE",
                110.0,
                111.0,
                110.5,
                "NIFTY-2026-12-03-25350-PE",
                95.0,
                96.0,
                95.5,
            ))
        }
    })
    .await;

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.poll_interval_seconds = 1800;
    settings.analysis.bar_minutes = 120;

    // Shared SQLite journal across Day 1 and Day 2
    let (_db, journal) = temp_journal();

    // Day 1 run
    let day1_pairs: BTreeMap<String, String> = dotenvy::from_path_iter(&env_path)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let day1_tok = day1_pairs["INDSTOCKS_ACCESS_TOKEN"].clone();
    let client1 = IndstocksClient::new(settings.indstocks.clone(), day1_tok);
    let orch1 =
        TradingAgentsOrchestrator::deterministic(settings.analysis.clone(), settings.llm.clone())
            .with_journal(journal.clone());
    let mut runner1 = PaperRunner::new(settings.clone(), orch1, journal.clone()).unwrap();
    let clock1 = FakeClock::new(ist_utc(2026, 11, 26, 14, 30, 0));

    let day1_summary = run_paper_session(
        &mut runner1,
        &client1,
        &clock1,
        clock_sleeper(clock1.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("day 1 completes");
    assert_eq!(day1_summary.date_ist, "2026-11-26");
    assert_eq!(day1_summary.eod_squared_off, 1);
    assert_eq!(journal.reflection_count().unwrap(), 1);

    // Rotate token in .env preserving GEMINI_API_KEY
    fs::write(
        &env_path,
        "GEMINI_API_KEY=keep-gemini-secret\nINDSTOCKS_ACCESS_TOKEN=day2-token\n",
    )
    .expect("rotate day2 .env");
    *expected_token.lock().unwrap() = "day2-token".to_string();

    let day2_pairs: BTreeMap<String, String> = dotenvy::from_path_iter(&env_path)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(day2_pairs["GEMINI_API_KEY"], "keep-gemini-secret");
    assert_eq!(day2_pairs["INDSTOCKS_ACCESS_TOKEN"], "day2-token");

    // Verify old day1 token is now rejected with 401
    assert!(client1
        .fetch_snapshot(ist_utc(2026, 11, 27, 9, 15, 0))
        .await
        .is_err());

    // Day 2 run recalls Day 1 reflection and runs cleanly to Day 2 EOD
    let client2 =
        IndstocksClient::new(settings.indstocks.clone(), day2_pairs["INDSTOCKS_ACCESS_TOKEN"].clone());
    let orch2 =
        TradingAgentsOrchestrator::deterministic(settings.analysis.clone(), settings.llm.clone())
            .with_journal(journal.clone());
    let mut runner2 = PaperRunner::new(settings, orch2, journal.clone()).unwrap();
    let day2_open = ist_utc(2026, 11, 27, 9, 15, 0);
    use nifty_paper_agent::data::MarketDataFeed;
    let day2_snap = client2.fetch_snapshot(day2_open).await.expect("day2 fetch");
    let step_day2 = runner2.step_at(&day2_snap, day2_open).await.expect("day2 step");
    assert!(
        !step_day2.trace.retrieved_memories.is_empty(),
        "Day 2 must recall Day 1 reflection memory"
    );

    let clock2 = FakeClock::new(ist_utc(2026, 11, 27, 15, 0, 0));
    let day2_summary = run_paper_session(
        &mut runner2,
        &client2,
        &clock2,
        clock_sleeper(clock2.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("day 2 session completes");
    assert_eq!(day2_summary.date_ist, "2026-11-27");
    assert_eq!(day2_summary.open_positions_remaining, 0);
    assert!(journal.reflection_count().unwrap() >= 2);
}

#[tokio::test]
async fn t4_scenario_5_expired_token_and_missing_token_fail_fast_recovery() {
    // Part A: CLI fail-fast when INDSTOCKS_ACCESS_TOKEN is missing
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cli_settings = load_repo_settings();
    cli_settings.indstocks.market_data_enabled = true;
    cli_settings.llm.use_offline_deterministic = true;
    write_isolated_settings(dir.path(), &cli_settings);

    let out = Command::new(bin_exe())
        .current_dir(dir.path())
        .args(["paper", "--offline"])
        .env_remove("INDSTOCKS_ACCESS_TOKEN")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("spawn cli");
    assert!(!out.status.success());
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(msg.contains("INDSTOCKS_ACCESS_TOKEN"));
    assert!(msg.contains("scripts/update-indstocks-token"));

    // Part B: Mid-day 401 TokenException aborts session cleanly after squaring off open position
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
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
        )),
        MockHttpResponse::with_status(
            "401 Unauthorized",
            r#"{"status":"error","error_type":"TokenException","message":"Token expired at midday"}"#,
        ),
    ])
    .await;

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 5;
    let (_db, journal, mut runner) = make_runner_deterministic(settings.clone());
    let client = IndstocksClient::new(settings.indstocks, "midday-expiring-tok".to_string());
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 11, 0, 0));

    let err = run_paper_session(
        &mut runner,
        &client,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .unwrap_err();
    let err_str = format!("{err:#}");
    assert!(err_str.contains("401"));
    assert!(err_str.contains("TokenException"));
    assert!(err_str.contains("update-indstocks-token"));
    assert_eq!(runner.broker.open_positions_count(), 0);
    assert_eq!(runner.closed_trades().len(), 1);
    assert_eq!(runner.closed_trades()[0].exit_reason, "eod_square_off");
    assert_eq!(journal.reflection_count().unwrap(), 1);
}

#[tokio::test]
async fn t4_scenario_6_graceful_ctrl_c_shutdown_mid_session_squares_off_cleanly() {
    assert!(!LIVE_ORDER_PLACEMENT_IMPLEMENTED);

    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 5;
    let (_db, journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 10, 30, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-25400-CE", 120.0, 121.0, 120.5, now)],
            "mid-session ctrl-c scenario",
        ))
    });

    let notify = Arc::new(Notify::new());
    let notify_c = Arc::clone(&notify);
    let tick_counter = Arc::new(AtomicUsize::new(0));
    let tc = Arc::clone(&tick_counter);
    let clk_c = clock.clone();

    let sleeper = move |d: std::time::Duration| {
        let clk = clk_c.clone();
        let n = Arc::clone(&notify_c);
        let counter = Arc::clone(&tc);
        async move {
            clk.advance(Duration::from_std(d).unwrap());
            if counter.fetch_add(1, Ordering::SeqCst) == 1 {
                n.notify_one();
            }
        }
    };

    let summary = run_paper_session(&mut runner, &feed, &clock, sleeper, notify.notified())
        .await
        .expect("graceful shutdown succeeds");

    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.total_trades, 1);
    assert_eq!(summary.closed_trades[0].exit_reason, "eod_square_off");
    assert_eq!(journal.fill_count().unwrap(), 2);
    assert_eq!(journal.reflection_count().unwrap(), 1);
    let text = summary.format_summary();
    assert!(text.contains("NIFTY-25400-CE"));
    assert!(text.contains("Open Remaining: 0"));
}
