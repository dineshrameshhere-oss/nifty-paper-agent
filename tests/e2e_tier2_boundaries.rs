mod common;

use chrono::{Duration, NaiveDate};
use common::{
    bin_exe, buy_signal, clock_sleeper, ist_utc, load_repo_settings, make_quote,
    make_runner_deterministic, make_runner_with_backend, make_snapshot, repo_root,
    sample_expiries_json, sample_option_chain_json, test_settings, write_isolated_settings,
    MockHttpResponse, MockIndstocksServer, ScriptedFeed, ScriptedLlmBackend,
};
use nifty_paper_agent::{
    clock::{ist_date, market_hours_status, FakeClock, MarketHoursStatus},
    config::{Indstocks, Settings},
    data::{ensure_fresh_at, validate_quote_at, IndstocksClient, MarketDataFeed},
    paper_runner::run_paper_session,
    strategy::{IndependentStrategy, NoopStrategy},
    types::{MarketSnapshot, TraderProposal},
};
use serde_json::json;
use std::{
    fs,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

// ============================================================================
// F1: INDstocks API Docs Boundaries & Caveats — 5 Tier-2 Tests
// ============================================================================

#[test]
fn t2_f1_docs_warns_smart_order_is_tsl_caveat() {
    let text = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    assert!(text.contains("is_tsl"));
    assert!(text.contains("tsl_step_size"));
    assert!(text.contains("NOT YET LIVE") || text.contains("not yet live"));
    assert!(text.contains("silently ignored"));
}

#[test]
fn t2_f1_docs_warns_get_with_json_body_and_http_512_quirks() {
    let text = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    assert!(text.contains("512"));
    assert!(text.contains("InternalServerException"));
    assert!(text.contains("GET /order"));
    assert!(text.contains("GET /margin"));
    assert!(text.contains("JSON body"));
}

#[test]
fn t2_f1_docs_documents_token_exception_401_403_and_error_codes() {
    let text = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    for needle in [
        "TokenException",
        "401",
        "403",
        "400",
        "429",
        "InputException",
        "DataException",
    ] {
        assert!(text.contains(needle), "missing error doc: {needle}");
    }
}

#[test]
fn t2_f1_docs_explicitly_states_live_orders_disabled_in_paper_mode() {
    let docs = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    let readme = fs::read_to_string(repo_root().join("README.md")).expect("read README.md");
    assert!(docs.contains("LIVE_ORDER_PLACEMENT_IMPLEMENTED"));
    assert!(docs.contains("Zero live order"));
    assert!(readme.contains("LIVE_ORDER_PLACEMENT_IMPLEMENTED = false"));
    assert!(readme.contains("zero live orders"));
}

#[test]
fn t2_f1_docs_no_placeholder_replace_with_tokens_in_docs() {
    let docs = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    let readme = fs::read_to_string(repo_root().join("README.md")).expect("read README.md");
    assert!(!docs.contains("REPLACE_WITH_VERIFIED_"));
    assert!(!readme.contains("REPLACE_WITH_VERIFIED_"));
}

// ============================================================================
// F2: Live Order Placement Guardrail Boundaries — 5 Tier-2 Tests
// ============================================================================

#[test]
fn t2_f2_cargo_manifest_has_no_broker_order_sdk() {
    let cargo = fs::read_to_string(repo_root().join("Cargo.toml")).expect("read Cargo.toml");
    for forbidden in ["kiteconnect", "smartapi", "dhan", "upstox", "indstocks-sdk"] {
        assert!(!cargo.to_ascii_lowercase().contains(forbidden));
    }
}

#[tokio::test]
async fn t2_f2_noop_strategy_produces_no_external_side_effects() {
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    let snap = make_snapshot(
        now,
        vec![make_quote("NIFTY-25350-CE", 100.0, 101.0, 100.5, now)],
        "noop",
    );
    let sig = NoopStrategy
        .evaluate(&snap)
        .await
        .expect("noop evaluate ok");
    assert!(sig.is_none());
}

#[tokio::test]
async fn t2_f2_square_off_all_executes_locally_in_paper_broker() {
    let (_db, journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 14, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, t0)], "buy");
    runner.step_at(&s0, t0).await.expect("entry");
    assert_eq!(runner.broker.open_positions_count(), 1);

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let s_eod = make_snapshot(t_eod, vec![make_quote("NIFTY-CE", 105.0, 106.0, 105.5, t_eod)], "eod");
    let fills = runner.square_off_all(&s_eod, t_eod).expect("local square_off");
    assert_eq!(fills.len(), 1);
    assert_eq!(runner.broker.open_positions_count(), 0);
    assert_eq!(journal.fill_count().unwrap(), 2);
}

#[test]
fn t2_f2_indstocks_config_has_no_order_placement_fields() {
    let cfg = Indstocks::default();
    let json_val = serde_json::to_value(&cfg).expect("serialize Indstocks");
    let obj = json_val.as_object().expect("json object");
    for key in obj.keys() {
        assert!(
            !key.contains("order") && !key.contains("margin") && !key.contains("portfolio"),
            "unexpected order/margin/portfolio config field: {key}"
        );
    }
}

#[tokio::test]
async fn t2_f2_mock_server_rejects_any_non_get_request_during_session() {
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

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.poll_interval_seconds = 900;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings.clone());
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 0, 0));

    let summary = run_paper_session(
        &mut runner,
        &client,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(server.non_get_rejections(), 0);
}

// ============================================================================
// F3: Config & `settings.toml` Boundaries — 5 Tier-2 Tests
// ============================================================================

#[test]
fn t2_f3_expiry_override_none_and_some_both_deserialize() {
    let base = load_repo_settings();
    assert_eq!(base.indstocks.expiry_override, None);

    let mut raw = fs::read_to_string(repo_root().join("settings.toml")).expect("read settings.toml");
    raw.push_str("\nexpiry_override = \"2026-12-03\"\n");
    let parsed: Settings = toml::from_str(&raw).expect("parse with expiry_override");
    assert_eq!(
        parsed.indstocks.expiry_override.as_deref(),
        Some("2026-12-03")
    );
}

#[test]
fn t2_f3_strike_count_and_poll_interval_boundary_values() {
    let mut s = load_repo_settings();
    s.indstocks.strike_count = 1;
    s.indstocks.poll_interval_seconds = 1;
    let serialized = toml::to_string(&s).expect("serialize");
    let roundtrip: Settings = toml::from_str(&serialized).expect("deserialize min");
    assert_eq!(roundtrip.indstocks.strike_count, 1);
    assert_eq!(roundtrip.indstocks.poll_interval_seconds, 1);

    s.indstocks.strike_count = 50;
    s.indstocks.poll_interval_seconds = 60;
    let serialized_max = toml::to_string(&s).expect("serialize max");
    let roundtrip_max: Settings = toml::from_str(&serialized_max).expect("deserialize max");
    assert_eq!(roundtrip_max.indstocks.strike_count, 50);
    assert_eq!(roundtrip_max.indstocks.poll_interval_seconds, 60);
}

#[test]
fn t2_f3_missing_settings_file_returns_clear_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does_not_exist.toml");
    let err = Settings::load(&missing).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("settings.toml"), "msg={msg}");
}

#[test]
fn t2_f3_malformed_toml_returns_parse_error() {
    let bad_toml = r#"
[paper]
starting_capital_paise = "not-an-integer"
"#;
    let res: Result<Settings, _> = toml::from_str(bad_toml);
    assert!(res.is_err());
}

#[tokio::test]
async fn t2_f3_trailing_slash_rest_base_url_normalizes_cleanly() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
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
        )),
    ])
    .await;

    let trailing_url = format!("{}/", server.base_url());
    let settings = test_settings(Some(&trailing_url));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .expect("fetch succeeds with trailing slash");

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert!(!reqs[0].path.contains("//"), "double slash: {}", reqs[0].path);
    assert!(!reqs[1].path.contains("//"), "double slash: {}", reqs[1].path);
}

// ============================================================================
// F4: Token Validation & HTTP Error Boundaries — 5 Tier-2 Tests
// ============================================================================

#[tokio::test]
async fn t2_f4_empty_token_rejected_before_network_call() {
    let server = MockIndstocksServer::start_queued(vec![]).await;
    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, String::new());
    let err = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("INDSTOCKS_ACCESS_TOKEN"));
    assert_eq!(server.request_count(), 0);
}

#[tokio::test]
async fn t2_f4_whitespace_and_newline_tokens_rejected_before_network_call() {
    let server = MockIndstocksServer::start_queued(vec![]).await;
    let settings = test_settings(Some(server.base_url()));
    for bad in ["   \t ", "tok\nline2", "tok\r\nline2", "REPLACE_WITH_TOKEN"] {
        let client = IndstocksClient::new(settings.indstocks.clone(), bad.to_string());
        let err = client
            .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("INDSTOCKS_ACCESS_TOKEN"),
            "bad={bad:?} err={err}"
        );
    }
    assert_eq!(server.request_count(), 0);
}

#[tokio::test]
async fn t2_f4_http_429_rate_limit_returns_error() {
    let server = MockIndstocksServer::start_queued(vec![MockHttpResponse::with_status(
        "429 Too Many Requests",
        r#"{"error":"Rate limit exceeded","success":false}"#,
    )])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let err = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("429"), "msg={msg}");
    assert!(msg.contains("rate limit"), "msg={msg}");
}

#[tokio::test]
async fn t2_f4_http_400_and_500_return_structured_error() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::with_status(
            "400 Bad Request",
            r#"{"message":"Bad Request","debug_info":"Invalid expiry passed"}"#,
        ),
        MockHttpResponse::with_status(
            "500 Internal Server Error",
            r#"{"status":"error","message":"Upstream failure"}"#,
        ),
    ])
    .await;

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.expiry_override = Some("2026-12-03".to_string());
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());

    let err_400 = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .unwrap_err();
    assert!(format!("{err_400:#}").contains("400"));
    assert!(format!("{err_400:#}").contains("Invalid expiry passed"));

    let err_500 = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 5))
        .await
        .unwrap_err();
    assert!(format!("{err_500:#}").contains("500"));
}

#[tokio::test]
async fn t2_f4_error_message_never_leaks_secret_token() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::with_status(
            "401 Unauthorized",
            r#"{"status":"error","error_type":"TokenException","message":"Invalid token"}"#,
        ),
        MockHttpResponse::with_status("500 Internal Server Error", "server oops"),
    ])
    .await;

    let secret = "SECRET_TOKEN_XYZ_987";
    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, secret.to_string());

    for _ in 0..2 {
        let err = client
            .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
            .await
            .unwrap_err();
        assert!(!format!("{err:#}").contains(secret));
    }
}

// ============================================================================
// F5: Option Chain & Quote Parsing Boundaries — 5 Tier-2 Tests
// ============================================================================

#[tokio::test]
async fn t2_f5_caches_discovered_expiry_within_same_ist_date() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
        if req.path == "/market/instruments/expiries" {
            MockHttpResponse::ok(sample_expiries_json(&["2026-12-03", "2026-12-10"]))
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

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());

    let t1 = ist_utc(2026, 11, 27, 9, 20, 0);
    let t2 = ist_utc(2026, 11, 27, 14, 20, 0);
    client.fetch_snapshot(t1).await.expect("first fetch");
    client.fetch_snapshot(t2).await.expect("second fetch");

    let reqs = server.requests();
    let expiries_hits = reqs
        .iter()
        .filter(|r| r.path == "/market/instruments/expiries")
        .count();
    let chain_hits = reqs
        .iter()
        .filter(|r| r.path == "/market/option-chain")
        .count();
    assert_eq!(expiries_hits, 1, "expiry should be cached on same IST date");
    assert_eq!(chain_hits, 2);
}

#[tokio::test]
async fn t2_f5_refreshes_expiry_cache_when_ist_date_rolls_over() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
        if req.path == "/market/instruments/expiries" {
            MockHttpResponse::ok(sample_expiries_json(&["2026-12-03", "2026-12-10"]))
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

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());

    let day1 = ist_utc(2026, 11, 27, 23, 59, 0);
    let day2 = ist_utc(2026, 11, 28, 0, 1, 0);
    client.fetch_snapshot(day1).await.expect("day 1");
    client.fetch_snapshot(day2).await.expect("day 2");

    let expiries_hits = server
        .requests()
        .iter()
        .filter(|r| r.path == "/market/instruments/expiries")
        .count();
    assert_eq!(expiries_hits, 2, "expiry cache must invalidate on IST date rollover");
}

#[tokio::test]
async fn t2_f5_skips_illiquid_zero_bid_or_crossed_contracts_or_rejects_empty_chain() {
    let partial_invalid = json!({
        "status": "success",
        "data": {
            "underlying_ltp": 25350.0,
            "expiry": "2026-12-03",
            "strikes": {
                "25300": {
                    "ce": { "trading_symbol": "BAD-ZERO-BID", "top_bid_price": 0.0, "top_ask_price": 10.0, "last_price": 5.0 },
                    "pe": { "trading_symbol": "BAD-CROSSED", "top_bid_price": 50.0, "top_ask_price": 45.0, "last_price": 48.0 }
                },
                "25350": {
                    "ce": { "trading_symbol": "GOOD-CE", "top_bid_price": 100.0, "top_ask_price": 101.0, "last_price": 100.5 }
                }
            }
        }
    })
    .to_string();

    let all_invalid = json!({
        "status": "success",
        "data": {
            "underlying_ltp": 25350.0,
            "expiry": "2026-12-03",
            "strikes": {
                "25300": {
                    "ce": { "trading_symbol": "ONLY-ZERO", "top_bid_price": 0.0, "top_ask_price": 5.0, "last_price": 2.0 }
                }
            }
        }
    })
    .to_string();

    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(partial_invalid),
        MockHttpResponse::ok(all_invalid),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let now = ist_utc(2026, 11, 27, 10, 0, 0);

    let snap = client.fetch_snapshot(now).await.expect("filters invalid legs");
    assert_eq!(snap.quotes.len(), 1);
    assert_eq!(snap.quotes[0].symbol, "GOOD-CE");

    let err = client.fetch_snapshot(now).await.unwrap_err();
    assert!(err.to_string().contains("no valid tradable quotes"));
}

#[tokio::test]
async fn t2_f5_malformed_json_or_missing_underlying_ltp_returns_error() {
    let zero_spot = sample_option_chain_json(
        0.0,
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
    );
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok("{not-json"),
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(zero_spot),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let now = ist_utc(2026, 11, 27, 10, 0, 0);

    assert!(client.fetch_snapshot(now).await.is_err());
    let err = client.fetch_snapshot(now).await.unwrap_err();
    assert!(err.to_string().contains("underlying_ltp"));
}

#[tokio::test]
async fn t2_f5_market_data_disabled_rejects_fetch_without_http_call() {
    let server = MockIndstocksServer::start_queued(vec![]).await;
    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.market_data_enabled = false;
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());

    let err = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("disabled"));
    assert_eq!(server.request_count(), 0);
}

// ============================================================================
// F6: Clock & IST Market Hours Boundaries — 5 Tier-2 Tests
// ============================================================================

#[test]
fn t2_f6_exact_0915_and_1530_ist_boundaries() {
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 9, 14, 59)),
        MarketHoursStatus::PreMarket
    );
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 9, 15, 0)),
        MarketHoursStatus::Open
    );
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 15, 29, 59)),
        MarketHoursStatus::Open
    );
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 15, 30, 0)),
        MarketHoursStatus::Closed
    );
}

#[test]
fn t2_f6_ist_midnight_utc_date_boundary() {
    // 18:45:00 UTC on Nov 27 = 00:15:00 IST on Nov 28
    use chrono::TimeZone;
    let utc = chrono::Utc.with_ymd_and_hms(2026, 11, 27, 18, 45, 0).unwrap();
    assert_eq!(ist_date(utc), NaiveDate::from_ymd_opt(2026, 11, 28).unwrap());
    assert_eq!(market_hours_status(utc), MarketHoursStatus::PreMarket);
}

#[test]
fn t2_f6_ensure_fresh_at_exact_max_age_vs_plus_one_sec() {
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    let q_ok = make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now - Duration::seconds(15));
    assert!(ensure_fresh_at(&q_ok, 15, now).is_ok());

    let q_stale = make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now - Duration::seconds(16));
    assert!(ensure_fresh_at(&q_stale, 15, now).is_err());
}

#[test]
fn t2_f6_validate_quote_at_rejects_future_beyond_5s_and_bad_prices() {
    let now = ist_utc(2026, 11, 27, 10, 0, 0);

    let future_bad = make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now + Duration::seconds(6));
    assert!(validate_quote_at(&future_bad, now).is_err());

    let future_ok = make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now + Duration::seconds(5));
    assert!(validate_quote_at(&future_ok, now).is_ok());

    let nan_q = make_quote("NIFTY-CE", f64::NAN, 101.0, 100.5, now);
    assert!(validate_quote_at(&nan_q, now).is_err());

    let zero_bid = make_quote("NIFTY-CE", 0.0, 101.0, 100.5, now);
    assert!(validate_quote_at(&zero_bid, now).is_err());

    let crossed = make_quote("NIFTY-CE", 102.0, 101.0, 101.5, now);
    assert!(validate_quote_at(&crossed, now).is_err());

    let blank_sym = make_quote("  ", 100.0, 101.0, 100.5, now);
    assert!(validate_quote_at(&blank_sym, now).is_err());
}

#[tokio::test]
async fn t2_f6_indstocks_client_implements_market_data_feed_trait_object() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
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
        )),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let real_feed: Arc<dyn MarketDataFeed> =
        Arc::new(IndstocksClient::new(settings.indstocks, "tok".to_string()));
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    let s1 = real_feed.fetch_snapshot(now).await.expect("trait call");
    assert_eq!(s1.quotes.len(), 2);

    let fake_feed: Arc<dyn MarketDataFeed> = Arc::new(ScriptedFeed::new(|_i, ts| {
        Ok(make_snapshot(
            ts,
            vec![make_quote("SYNTH-CE", 50.0, 51.0, 50.5, ts)],
            "synth",
        ))
    }));
    let s2 = fake_feed.fetch_snapshot(now).await.expect("fake trait call");
    assert_eq!(s2.quotes[0].symbol, "SYNTH-CE");
}

// ============================================================================
// F7: Deterministic Tick Monitoring Boundaries — 5 Tier-2 Tests
// ============================================================================

#[tokio::test]
async fn t2_f7_trailing_stop_never_ratchets_downward_on_dip() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            // entry=101, tsl=91 -> trailing_distance=10
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal("tsl-dip", &q.symbol, 2, 101.0, 75.0, 91.0, 160.0)),
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
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, t0)], "open");
    runner.step_at(&s0, t0).await.expect("entry");

    // Rally to 125 -> TSL ratchets to 115
    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 124.0, 126.0, 125.0, t1)], "high");
    assert!(runner.monitor_positions_at(&s1, t1).unwrap().is_empty());
    assert!((runner.active_position_context(&s1).unwrap().trailing_stop_loss_price - 115.0).abs() < 1e-6);

    // Dip to 118 (above 115) -> TSL must remain 115, NOT drop to 108
    let t2 = t1 + Duration::seconds(5);
    let s2 = make_snapshot(t2, vec![make_quote("NIFTY-CE", 118.0, 119.0, 118.0, t2)], "dip");
    assert!(runner.monitor_positions_at(&s2, t2).unwrap().is_empty());
    assert!((runner.active_position_context(&s2).unwrap().trailing_stop_loss_price - 115.0).abs() < 1e-6);
}

#[tokio::test]
async fn t2_f7_exact_equality_at_sl_tsl_and_target_triggers_exit() {
    for (label, exit_bid, expected_reason) in [
        ("exact_sl", 80.0, "stop_loss"),
        ("exact_tsl", 90.0, "trailing_stop_loss"),
        ("exact_target", 130.0, "target_hit"),
    ] {
        let backend = Arc::new(ScriptedLlmBackend::new(move |snap, pos, _| {
            if pos.is_none() {
                let q = &snap.quotes[0];
                TraderProposal {
                    action: "BUY".into(),
                    rationale: "enter".into(),
                    confidence_bps: 8000,
                    signal: Some(buy_signal(
                        &format!("eq-{label}"),
                        &q.symbol,
                        1,
                        102.0,
                        80.0,
                        90.0,
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
        let (_db, _journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
        let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
        let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, t0)], "entry");
        runner.step_at(&s0, t0).await.expect("entry");

        let t1 = t0 + Duration::seconds(5);
        let s1 = make_snapshot(
            t1,
            vec![make_quote("NIFTY-CE", exit_bid, exit_bid + 0.5, exit_bid, t1)],
            label,
        );
        let fills = runner.monitor_positions_at(&s1, t1).expect("exit");
        assert_eq!(fills.len(), 1, "failed on {label}");
        assert_eq!(runner.closed_trades()[0].exit_reason, expected_reason);
    }
}

#[tokio::test]
async fn t2_f7_tick_missing_quote_for_open_symbol_does_not_panic_or_close() {
    let (_db, _journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("OPEN-SYM", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");
    assert_eq!(runner.broker.open_position_qty("OPEN-SYM"), 1);

    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("OTHER-SYM", 50.0, 51.0, 50.5, t1)], "missing");
    let fills = runner.monitor_positions_at(&s1, t1).expect("no panic");
    assert!(fills.is_empty());
    assert_eq!(runner.broker.open_position_qty("OPEN-SYM"), 1);
}

#[tokio::test]
async fn t2_f7_stop_loss_fill_accepted_by_paper_broker_below_original_entry() {
    let mut settings = test_settings(None);
    settings.paper.spread_bps = 25;
    settings.paper.slippage_bps = 15;
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal("sl-spread", &q.symbol, 2, 102.0, 70.0, 85.0, 150.0)),
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
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.2, 100.1, t0)], "open");
    runner.step_at(&s0, t0).await.expect("entry");

    let t1 = t0 + Duration::seconds(5);
    // Zero-spread quote at SL=70.0 -> simulated sell px is slightly below 70.0 due to slippage,
    // protective_sell_limit must let PaperBroker accept it!
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 70.0, 70.0, 70.0, t1)], "sl");
    let fills = runner.monitor_positions_at(&s1, t1).expect("sl accepted");
    assert_eq!(fills.len(), 1);
    assert!(fills[0].price < 70.0);
    assert_eq!(runner.broker.open_positions_count(), 0);
}

#[tokio::test]
async fn t2_f7_daily_loss_paise_accumulates_on_tick_sl_exit() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal("loss-acc", &q.symbol, 5, 102.0, 80.0, 90.0, 150.0)),
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
    assert_eq!(runner.daily_loss_paise(), 0);

    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.0, 100.0, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");

    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 80.0, 80.0, 80.0, t1)], "sl");
    runner.monitor_positions_at(&s1, t1).expect("sl exit");
    assert!(runner.daily_loss_paise() >= 10_000, "loss={}", runner.daily_loss_paise());
}

// ============================================================================
// F8: Continuous Session Resilience & Risk Limits — 5 Tier-2 Tests
// ============================================================================

#[tokio::test]
async fn t2_f8_transient_feed_error_during_session_does_not_abort_or_corrupt() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 60;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 27, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        if idx == 1 {
            anyhow::bail!("transient 503 network glitch");
        }
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now)],
            "ok",
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
    .expect("session survives transient error");

    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.eod_squared_off, 1);
}

#[tokio::test]
async fn t2_f8_stale_snapshot_from_feed_is_rejected_without_trading() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 5;
    settings.analysis.stale_after_seconds = 15;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 28, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        let stale_ts = now - Duration::seconds(60);
        Ok(MarketSnapshot {
            id: uuid::Uuid::new_v4(),
            created_at: stale_ts,
            quotes: vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, stale_ts)],
            context: "always stale".into(),
        })
    });

    let summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("stale snapshots safely skipped");

    assert_eq!(summary.total_trades, 0);
    assert_eq!(summary.starting_cash_paise, summary.ending_cash_paise);
}

#[tokio::test]
async fn t2_f8_daily_loss_limit_blocks_new_buys_for_rest_of_session() {
    let mut settings = test_settings(None);
    settings.paper.max_daily_loss_paise = 5_000; // ₹50 cap
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 1;

    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, idx| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "attempt".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    &format!("dl-cap-{idx}"),
                    &q.symbol,
                    3,
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

    let (_db, _journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 25, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        // Tick 0 opens at ~100, Tick 1 drops to 80 -> SL loss = ~₹60 (> ₹50 cap),
        // Ticks 2..4 recover to 100 -> new buys must be blocked by daily loss cap!
        let px = if idx == 1 { 80.0 } else { 100.0 };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", px, px + 0.5, px, now)],
            "daily loss cap test",
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

    assert_eq!(summary.total_trades, 1, "expected only 1 trade before cap triggered");
    assert_eq!(summary.closed_trades[0].exit_reason, "stop_loss");
    assert!(runner.daily_loss_paise() >= 5_000);
}

#[tokio::test]
async fn t2_f8_max_open_positions_blocks_excess_entries_across_ticks() {
    let mut settings = test_settings(None);
    settings.paper.max_open_positions = 2;
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 1;

    let backend = Arc::new(ScriptedLlmBackend::new(|_snap, _pos, idx| {
        let sym = match idx {
            0 => "SYM-1",
            1 => "SYM-2",
            _ => "SYM-3",
        };
        TraderProposal {
            action: "BUY".into(),
            rationale: "multi".into(),
            confidence_bps: 8000,
            signal: Some(buy_signal(
                &format!("pos-cap-{idx}"),
                sym,
                1,
                102.0,
                80.0,
                90.0,
                150.0,
            )),
        }
    }));

    let (_db, _journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 25, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![
                make_quote("SYM-1", 100.0, 100.5, 100.2, now),
                make_quote("SYM-2", 100.0, 100.5, 100.2, now),
                make_quote("SYM-3", 100.0, 100.5, 100.2, now),
            ],
            "multi-symbol",
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

    assert_eq!(summary.eod_squared_off, 2, "3rd symbol must be blocked by max_open_positions=2");
    assert_eq!(summary.total_trades, 2);
}

#[tokio::test]
async fn t2_f8_duplicate_signal_ids_skipped_across_continuous_ticks() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 1;

    let backend = Arc::new(ScriptedLlmBackend::new(|snap, _pos, _idx| {
        let q = &snap.quotes[0];
        TraderProposal {
            action: "BUY".into(),
            rationale: "fixed id".into(),
            confidence_bps: 8000,
            signal: Some(buy_signal(
                "CONSTANT-DUPLICATE-ID",
                &q.symbol,
                1,
                102.0,
                80.0,
                90.0,
                150.0,
            )),
        }
    }));

    let (_db, journal, mut runner) = make_runner_with_backend(settings, backend);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 26, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, now)],
            "dup id",
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
    .expect("session completes cleanly");

    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.total_trades, 1);
    assert_eq!(journal.fill_count().unwrap(), 2); // 1 buy + 1 eod sell
}

// ============================================================================
// F9: EOD Square-Off & DailySummary Boundaries — 5 Tier-2 Tests
// ============================================================================

#[test]
fn t2_f9_square_off_all_when_flat_is_noop() {
    let (_db, _journal, mut runner) = make_runner_deterministic(test_settings(None));
    let now = ist_utc(2026, 11, 27, 15, 30, 0);
    let snap = make_snapshot(now, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now)], "flat");
    let fills = runner.square_off_all(&snap, now).expect("noop");
    assert!(fills.is_empty());

    let summary = runner.daily_summary(now);
    assert_eq!(summary.eod_squared_off, 0);
    assert_eq!(summary.total_trades, 0);
    assert_eq!(summary.open_positions_remaining, 0);
}

#[tokio::test]
async fn t2_f9_square_off_all_closes_multiple_open_positions() {
    let call_num = Arc::new(AtomicUsize::new(0));
    let cn = Arc::clone(&call_num);
    let backend = Arc::new(ScriptedLlmBackend::new(move |_snap, _pos, _| {
        let i = cn.fetch_add(1, Ordering::SeqCst);
        let sym = match i {
            0 => "OPT-A",
            1 => "OPT-B",
            _ => "OPT-C",
        };
        TraderProposal {
            action: "BUY".into(),
            rationale: "open".into(),
            confidence_bps: 8000,
            signal: Some(buy_signal(
                &format!("multi-{i}"),
                sym,
                1,
                102.0,
                80.0,
                90.0,
                150.0,
            )),
        }
    }));

    let (_db, _journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
    let t0 = ist_utc(2026, 11, 27, 14, 0, 0);
    let snap = make_snapshot(
        t0,
        vec![
            make_quote("OPT-A", 100.0, 100.5, 100.2, t0),
            make_quote("OPT-B", 100.0, 100.5, 100.2, t0),
            make_quote("OPT-C", 100.0, 100.5, 100.2, t0),
        ],
        "three positions",
    );
    runner.step_at(&snap, t0).await.unwrap();
    runner.step_at(&snap, t0).await.unwrap();
    runner.step_at(&snap, t0).await.unwrap();
    assert_eq!(runner.broker.open_positions_count(), 3);

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let eod_snap = make_snapshot(
        t_eod,
        vec![
            make_quote("OPT-A", 105.0, 106.0, 105.5, t_eod),
            make_quote("OPT-B", 110.0, 111.0, 110.5, t_eod),
            make_quote("OPT-C", 95.0, 96.0, 95.5, t_eod),
        ],
        "eod",
    );
    let fills = runner.square_off_all(&eod_snap, t_eod).expect("batch square off");
    assert_eq!(fills.len(), 3);
    let summary = runner.daily_summary(t_eod);
    assert_eq!(summary.eod_squared_off, 3);
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.total_trades, 3);
}

#[tokio::test]
async fn t2_f9_square_off_falls_back_to_cached_quote_when_eod_snapshot_empty() {
    let (_db, _journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 14, 50, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");
    assert_eq!(runner.broker.open_position_qty("NIFTY-CE"), 1);

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let empty_eod = make_snapshot(t_eod, vec![], "empty eod fallback");
    let fills = runner
        .square_off_all(&empty_eod, t_eod)
        .expect("cached last_quotes fallback succeeds");
    assert_eq!(fills.len(), 1);
    assert_eq!(runner.broker.open_positions_count(), 0);
}

#[tokio::test]
async fn t2_f9_daily_summary_cash_delta_matches_realized_pnl_when_flat() {
    let (_db, _journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");

    let t1 = ist_utc(2026, 11, 27, 15, 30, 0);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 115.0, 116.0, 115.5, t1)], "eod");
    runner.square_off_all(&s1, t1).expect("square off");

    let summary = runner.daily_summary(t1);
    assert_eq!(summary.open_positions_remaining, 0);
    let cash_delta = summary.ending_cash_paise - summary.starting_cash_paise;
    assert!(
        (cash_delta - summary.realized_pnl_paise).abs() <= 2,
        "cash_delta={cash_delta}, realized_pnl={}",
        summary.realized_pnl_paise
    );
}

#[tokio::test]
async fn t2_f9_shutdown_signal_squares_off_open_positions_before_exit() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    let (_db, journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 10, 0, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now)],
            "open",
        ))
    });

    let notify = Arc::new(Notify::new());
    let notify_c = Arc::clone(&notify);
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticks_c = Arc::clone(&ticks);
    let clk_c = clock.clone();

    let sleeper = move |d: std::time::Duration| {
        let clk = clk_c.clone();
        let n = Arc::clone(&notify_c);
        let tc = Arc::clone(&ticks_c);
        async move {
            clk.advance(Duration::from_std(d).unwrap());
            // Trigger shutdown on tick 1 AFTER tick 0 opened a position
            if tc.fetch_add(1, Ordering::SeqCst) == 0 {
                n.notify_one();
            }
        }
    };

    let summary = run_paper_session(&mut runner, &feed, &clock, sleeper, notify.notified())
        .await
        .expect("shutdown completes");
    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.closed_trades.len(), 1);
    assert_eq!(journal.fill_count().unwrap(), 2);
}

// ============================================================================
// F10: Token Refresh Scripts & CLI Boundaries — 5 Tier-2 Tests
// ============================================================================

#[test]
fn t2_f10_cli_paper_fails_fast_on_whitespace_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut settings = load_repo_settings();
    settings.indstocks.market_data_enabled = true;
    settings.llm.use_offline_deterministic = true;
    write_isolated_settings(dir.path(), &settings);

    let output = Command::new(bin_exe())
        .current_dir(dir.path())
        .args(["paper", "--offline"])
        .env("INDSTOCKS_ACCESS_TOKEN", "   \t  ")
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
}

#[test]
fn t2_f10_sh_and_ps1_reject_empty_and_multiline_tokens() {
    let sh = fs::read_to_string(repo_root().join("scripts").join("update-indstocks-token.sh"))
        .expect("read .sh");
    let ps1 = fs::read_to_string(repo_root().join("scripts").join("update-indstocks-token.ps1"))
        .expect("read .ps1");

    assert!(sh.contains("-z \"$token\""));
    assert!(sh.contains("$'\\n'") && sh.contains("$'\\r'"));
    assert!(ps1.contains("IsNullOrWhiteSpace"));
    assert!(ps1.contains("`r") && ps1.contains("`n"));
}

#[test]
fn t2_f10_sh_sets_umask_077_and_chmod_600() {
    let sh = fs::read_to_string(repo_root().join("scripts").join("update-indstocks-token.sh"))
        .expect("read .sh");
    assert!(sh.contains("umask 077"));
    assert!(sh.contains("chmod 600"));
    assert!(sh.contains("unset token"));
}

#[test]
fn t2_f10_cargo_toml_uses_rustls_and_bundled_sqlite() {
    let cargo = fs::read_to_string(repo_root().join("Cargo.toml")).expect("read Cargo.toml");
    assert!(cargo.contains("rustls-tls"));
    assert!(cargo.contains("default-features = false"));
    assert!(cargo.contains("rustls-tls-webpki-roots"));
    assert!(cargo.contains("\"bundled\""));
}

#[test]
fn t2_f10_default_cli_invocation_exits_zero_safely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = load_repo_settings();
    write_isolated_settings(dir.path(), &settings);

    let output = Command::new(bin_exe())
        .current_dir(dir.path())
        .env_remove("INDSTOCKS_ACCESS_TOKEN")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("spawn default cli");

    assert!(
        output.status.success(),
        "default cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
