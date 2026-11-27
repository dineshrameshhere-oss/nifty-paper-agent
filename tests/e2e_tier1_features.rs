mod common;

use chrono::{Duration, Timelike};
use common::{
    bin_exe, buy_signal, clock_sleeper, ist_utc, load_repo_settings, make_quote,
    make_runner_deterministic, make_runner_with_backend, make_snapshot, repo_root,
    sample_expiries_json, sample_option_chain_json, test_settings, write_isolated_settings,
    MockHttpResponse, MockIndstocksServer, ScriptedFeed, ScriptedLlmBackend,
};
use nifty_paper_agent::{
    clock::{ist_date, ist_offset, market_hours_status, to_ist, Clock, FakeClock, MarketHoursStatus},
    config::{Indstocks, Settings},
    data::{ensure_fresh_at, validate_quote_at, IndstocksClient, MarketDataFeed},
    paper_broker::PaperBroker,
    paper_runner::run_paper_session,
    risk,
    types::{Side, TraderProposal},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

// ============================================================================
// F1: INDstocks API Docs (Market Data + Live Trading) — 5 Tier-1 Tests
// ============================================================================

#[test]
fn t1_f1_docs_indstocks_api_exists_and_lists_market_data_endpoints() {
    let doc_path = repo_root().join("docs").join("indstocks-api.md");
    assert!(doc_path.exists(), "docs/indstocks-api.md must exist");
    let text = fs::read_to_string(&doc_path).expect("read docs/indstocks-api.md");

    for required in [
        "https://api.indstocks.com",
        "wss://ws-prices.indstocks.com/api/v1/ws/prices",
        "/market/instruments/expiries",
        "/market/option-chain",
        "/market/quotes/full",
        "/market/quotes/ltp",
    ] {
        assert!(
            text.contains(required),
            "docs/indstocks-api.md missing market-data reference: {required}"
        );
    }
}

#[test]
fn t1_f1_docs_documents_raw_authorization_header_and_rate_limits() {
    let text = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    assert!(text.contains("Authorization: <INDSTOCKS_ACCESS_TOKEN>"));
    assert!(text.contains("no `Bearer` prefix") || text.contains("NO `Bearer` Prefix"));
    assert!(text.contains("Rate Limits"));
    assert!(text.contains("100,000"));
    assert!(text.contains("429"));
}

#[test]
fn t1_f1_docs_documents_post_trial_live_order_apis() {
    let text = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    for endpoint in [
        "POST /order",
        "POST /order/modify",
        "POST /order/cancel",
        "GET /order-book",
        "GET /order",
        "GET /order/trades",
        "GET /trade-book",
        "POST /smart/order",
        "POST /smart/order/modify",
        "POST /smart/order/cancel",
        "wss://ws-order-updates.indstocks.com/api/v1/ws/trades",
    ] {
        assert!(
            text.contains(endpoint),
            "docs/indstocks-api.md missing live order endpoint: {endpoint}"
        );
    }
    assert!(text.contains("txn_type"));
    assert!(text.contains("security_id"));
    assert!(text.contains("order_type"));
}

#[test]
fn t1_f1_docs_documents_post_trial_portfolio_account_margin_apis() {
    let text = fs::read_to_string(repo_root().join("docs").join("indstocks-api.md"))
        .expect("read docs/indstocks-api.md");
    for endpoint in [
        "GET /portfolio/holdings",
        "GET /portfolio/positions",
        "GET /user/profile",
        "GET /funds",
        "GET /margin",
    ] {
        assert!(
            text.contains(endpoint),
            "docs/indstocks-api.md missing portfolio/account/margin endpoint: {endpoint}"
        );
    }
    assert!(text.contains("Position Square-Off Pattern"));
    assert!(text.contains("NSE/INVG/67858"));
}

#[test]
fn t1_f1_docs_readme_documents_two_week_paper_and_daily_token_workflow() {
    let readme =
        fs::read_to_string(repo_root().join("README.md")).expect("read README.md");
    for needle in [
        "docs/indstocks-api.md",
        "LIVE_ORDER_PLACEMENT_IMPLEMENTED = false",
        "09:15",
        "15:30",
        "update-indstocks-token.sh",
        "update-indstocks-token.ps1",
        "INDSTOCKS_ACCESS_TOKEN",
    ] {
        assert!(
            readme.contains(needle),
            "README.md missing workflow item: {needle}"
        );
    }
}

// ============================================================================
// F2: Live Order Placement Guardrail — 5 Tier-1 Tests
// ============================================================================

#[test]
fn t1_f2_const_live_order_placement_is_false() {
    assert!(!LIVE_ORDER_PLACEMENT_IMPLEMENTED);
}

#[tokio::test]
async fn t1_f2_paper_runner_constructs_and_steps_with_guardrail_intact() {
    assert!(!LIVE_ORDER_PLACEMENT_IMPLEMENTED);
    let settings = test_settings(None);
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    let snap = make_snapshot(
        now,
        vec![make_quote("NIFTY-25350-CE", 100.0, 101.0, 100.5, now)],
        "NIFTY spot=25350",
    );
    let res = runner.step_at(&snap, now).await.expect("step_at succeeds");
    assert!(res.trace.risk_verdict.approved);
    assert!(res.fill.is_some());
}

#[test]
fn t1_f2_source_contains_no_live_order_or_margin_endpoints() {
    let src_dir = repo_root().join("src");
    let forbidden = [
        "/order\"",
        "/order/",
        "/orders",
        "/smart/order",
        "/portfolio/",
        "/funds\"",
        "/margin\"",
        "place-order",
        "place_order",
    ];
    for entry in fs::read_dir(&src_dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let body = fs::read_to_string(&path).expect("read src file");
        for pat in forbidden {
            assert!(
                !body.contains(pat),
                "forbidden live order/margin endpoint '{pat}' found in {}",
                path.display()
            );
        }
        if path.file_name().and_then(|f| f.to_str()) == Some("data.rs") {
            assert!(
                !body.contains(".bearer_auth("),
                "src/data.rs must not use bearer_auth for INDstocks"
            );
        }
    }
}

#[tokio::test]
async fn t1_f2_indstocks_client_only_hits_market_data_paths() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
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

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "paper-token-001".to_string());
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    let snap = client.fetch_snapshot(now).await.expect("fetch_snapshot");
    assert!(!snap.quotes.is_empty());

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert_eq!(r.method, "GET", "only GET requests allowed");
        assert!(
            r.path.starts_with("/market/"),
            "unexpected non-market path: {}",
            r.path
        );
    }
    assert_eq!(server.non_get_rejections(), 0);
}

#[tokio::test]
async fn t1_f2_paper_session_never_invokes_non_market_http_methods() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
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
    settings.indstocks.poll_interval_seconds = 1800; // 30-min intervals
    settings.analysis.bar_minutes = 60;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings.clone());
    let client = IndstocksClient::new(settings.indstocks, "paper-token-002".to_string());
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 14, 30, 0));

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
    let reqs = server.requests();
    assert!(!reqs.is_empty());
    assert!(reqs
        .iter()
        .all(|r| r.method == "GET" && r.path.starts_with("/market/")));
}

// ============================================================================
// F3: Verified `settings.toml` & Backwards-Compatible `config::Indstocks` — 5 Tier-1 Tests
// ============================================================================

#[test]
fn t1_f3_settings_toml_loads_verified_indstocks_urls_and_enabled() {
    let s = load_repo_settings();
    assert_eq!(s.indstocks.rest_base_url, "https://api.indstocks.com");
    assert_eq!(
        s.indstocks.websocket_url,
        "wss://ws-prices.indstocks.com/api/v1/ws/prices"
    );
    assert!(s.indstocks.market_data_enabled);
    assert_eq!(s.indstocks.symbol, "NIFTY");
}

#[test]
fn t1_f3_settings_toml_loads_verified_market_paths_and_params() {
    let s = load_repo_settings();
    assert_eq!(s.indstocks.option_chain_path, "/market/option-chain");
    assert_eq!(s.indstocks.expiries_path, "/market/instruments/expiries");
    assert_eq!(s.indstocks.quote_path, "/market/quotes/full");
    assert_eq!(s.indstocks.exchange, "NSE");
    assert_eq!(s.indstocks.segment, "INDEX");
    assert_eq!(s.indstocks.underlying_scrip, "40000001");
    assert_eq!(s.indstocks.strike_count, 10);
    assert_eq!(s.indstocks.poll_interval_seconds, 5);
}

#[test]
fn t1_f3_settings_toml_has_zero_replace_with_placeholders() {
    let raw = fs::read_to_string(repo_root().join("settings.toml")).expect("read settings.toml");
    assert!(!raw.contains("REPLACE_WITH"));
    assert!(!raw.contains("SET_"));
}

#[test]
fn t1_f3_minimal_legacy_indstocks_toml_uses_sane_defaults() {
    let legacy_toml = r#"
[paper]
starting_capital_paise = 1000000
max_trade_value_paise = 200000
max_open_positions = 3
max_daily_loss_paise = 50000
max_position_quantity = 50
allow_short = false
spread_bps = 10
slippage_bps = 5
fee_bps = 3
partial_fill_fraction = 1.0

[analysis]
mode = "hybrid"
bar_minutes = 5
stale_after_seconds = 15

[llm]
base_url = "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
fallback_model = "gemini-3.5-flash-lite"
max_concurrent_requests = 10
requests_per_minute = 5
temperature = 0.2

[indstocks]
rest_base_url = "https://api.indstocks.com"
websocket_url = "wss://ws-prices.indstocks.com/api/v1/ws/prices"
quote_path = "/market/quotes/full"
market_data_enabled = true
symbol = "NIFTY"
"#;

    let s: Settings = toml::from_str(legacy_toml).expect("deserialize legacy TOML");
    assert_eq!(s.indstocks.option_chain_path, "/market/option-chain");
    assert_eq!(s.indstocks.expiries_path, "/market/instruments/expiries");
    assert_eq!(s.indstocks.exchange, "NSE");
    assert_eq!(s.indstocks.segment, "INDEX");
    assert_eq!(s.indstocks.underlying_scrip, "40000001");
    assert_eq!(s.indstocks.strike_count, 10);
    assert_eq!(s.indstocks.poll_interval_seconds, 5);
    assert_eq!(s.indstocks.expiry_override, None);
}

#[test]
fn t1_f3_indstocks_default_matches_verified_endpoints() {
    let def = Indstocks::default();
    assert_eq!(def.rest_base_url, "https://api.indstocks.com");
    assert_eq!(
        def.websocket_url,
        "wss://ws-prices.indstocks.com/api/v1/ws/prices"
    );
    assert_eq!(def.quote_path, "/market/quotes/full");
    assert_eq!(def.option_chain_path, "/market/option-chain");
    assert_eq!(def.expiries_path, "/market/instruments/expiries");
    assert!(!def.market_data_enabled);
    assert_eq!(def.symbol, "NIFTY");
}

// ============================================================================
// F4: Raw `Authorization` Header & Token Validation — 5 Tier-1 Tests
// ============================================================================

#[tokio::test]
async fn t1_f4_client_sends_raw_authorization_header_without_bearer() {
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
    let token = "raw-token-abc123";
    let client = IndstocksClient::new(settings.indstocks, token.to_string());
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    client.fetch_snapshot(now).await.expect("fetch_snapshot");

    let reqs = server.requests();
    assert!(!reqs.is_empty());
    let auth = reqs[0].headers.get("authorization").expect("auth header");
    assert_eq!(auth, token);
    assert!(!auth.starts_with("Bearer "));
}

#[tokio::test]
async fn t1_f4_expiries_and_option_chain_both_include_raw_auth_header() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(sample_option_chain_json(
            25400.0,
            "2026-12-03",
            "25400",
            "NIFTY-CE",
            105.0,
            106.0,
            105.5,
            "NIFTY-PE",
            95.0,
            96.0,
            95.5,
        )),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let token = "raw-token-chain-xyz";
    let client = IndstocksClient::new(settings.indstocks, token.to_string());
    let now = ist_utc(2026, 11, 27, 10, 5, 0);
    client.fetch_snapshot(now).await.expect("fetch_snapshot");

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].path, "/market/instruments/expiries");
    assert_eq!(reqs[1].path, "/market/option-chain");
    for req in &reqs {
        let auth = req.headers.get("authorization").expect("auth header");
        assert_eq!(auth, token);
        assert!(!auth.starts_with("Bearer "));
    }
}

#[tokio::test]
async fn t1_f4_fetch_quotes_includes_raw_auth_header() {
    let server = MockIndstocksServer::start_queued(vec![MockHttpResponse::ok(
        json!({
            "status": "success",
            "data": {
                "NFO_45108": {
                    "live_price": 125.5,
                    "market_depth": {
                        "depth": [{ "buy": { "price": 125.0 }, "sell": { "price": 126.0 } }]
                    }
                }
            }
        })
        .to_string(),
    )])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let token = "raw-token-quotes-777";
    let client = IndstocksClient::new(settings.indstocks, token.to_string());
    let now = ist_utc(2026, 11, 27, 10, 10, 0);
    let quotes = client
        .fetch_quotes(&["NFO_45108".to_string()], now)
        .await
        .expect("fetch_quotes");
    assert_eq!(quotes.len(), 1);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].headers.get("authorization").map(String::as_str),
        Some(token)
    );
}

#[tokio::test]
async fn t1_f4_http_401_maps_to_clear_token_error() {
    let server = MockIndstocksServer::start_queued(vec![MockHttpResponse::with_status(
        "401 Unauthorized",
        r#"{"status":"error","message":"Unable to validate user token","error_type":"TokenException"}"#,
    )])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "expired-tok".to_string());
    let err = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "msg={msg}");
    assert!(msg.contains("TokenException"), "msg={msg}");
    assert!(msg.contains("INDSTOCKS_ACCESS_TOKEN"), "msg={msg}");
}

#[tokio::test]
async fn t1_f4_http_403_maps_to_clear_auth_error() {
    let server = MockIndstocksServer::start_queued(vec![MockHttpResponse::with_status(
        "403 Forbidden",
        r#"{"message":"The provided access_token is either incorrect, expired, or has been revoked.","success":false}"#,
    )])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "revoked-tok".to_string());
    let err = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("403"), "msg={msg}");
    assert!(msg.contains("authentication failed"), "msg={msg}");
    assert!(msg.contains("update-indstocks-token"), "msg={msg}");
}

// ============================================================================
// F5: NIFTY Expiries, Spot & Option Chain + Quotes Integration — 5 Tier-1 Tests
// ============================================================================

#[tokio::test]
async fn t1_f5_discovers_nearest_expiry_and_fetches_option_chain() {
    let server = MockIndstocksServer::start_with_handler(true, |req| {
        if req.path == "/market/instruments/expiries" {
            // Deliberately unsorted so we verify client selects nearest >= today_ist
            MockHttpResponse::ok(sample_expiries_json(&[
                "2026-12-17",
                "2026-12-03",
                "2026-12-10",
            ]))
        } else {
            assert!(
                req.query.contains("expiry=2026-12-03"),
                "expected nearest expiry query, got {}",
                req.query
            );
            assert!(req.query.contains("exchange=NSE"));
            assert!(req.query.contains("segment=INDEX"));
            assert!(req.query.contains("underlying-scrip=40000001"));
            MockHttpResponse::ok(sample_option_chain_json(
                25350.0,
                "2026-12-03",
                "25350",
                "NIFTY-2026-12-03-25350-CE",
                120.0,
                121.0,
                120.5,
                "NIFTY-2026-12-03-25350-PE",
                100.0,
                101.0,
                100.5,
            ))
        }
    })
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let now = ist_utc(2026, 11, 27, 10, 0, 0);
    let snap = client.fetch_snapshot(now).await.expect("fetch_snapshot");

    assert_eq!(snap.quotes.len(), 2);
    assert!(snap.context.contains("expiry=2026-12-03"));
}

#[tokio::test]
async fn t1_f5_option_chain_parses_ce_and_pe_bid_ask_last_and_symbol() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(sample_option_chain_json(
            25412.25,
            "2026-12-03",
            "25400",
            "NIFTY 03 DEC 25400 CE",
            142.10,
            143.40,
            142.80,
            "NIFTY 03 DEC 25400 PE",
            118.25,
            119.00,
            118.60,
        )),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let now = ist_utc(2026, 11, 27, 10, 15, 0);
    let snap = client.fetch_snapshot(now).await.expect("fetch_snapshot");

    let ce = snap
        .quotes
        .iter()
        .find(|q| q.symbol == "NIFTY 03 DEC 25400 CE")
        .expect("CE quote");
    assert!((ce.bid - 142.10).abs() < 1e-9);
    assert!((ce.ask - 143.40).abs() < 1e-9);
    assert!((ce.last - 142.80).abs() < 1e-9);
    assert_eq!(ce.timestamp, now);

    let pe = snap
        .quotes
        .iter()
        .find(|q| q.symbol == "NIFTY 03 DEC 25400 PE")
        .expect("PE quote");
    assert!((pe.bid - 118.25).abs() < 1e-9);
    assert!((pe.ask - 119.00).abs() < 1e-9);
    assert!((pe.last - 118.60).abs() < 1e-9);
    assert_eq!(pe.timestamp, now);
}

#[tokio::test]
async fn t1_f5_snapshot_context_includes_spot_expiry_atm_and_pcr() {
    let server = MockIndstocksServer::start_queued(vec![
        MockHttpResponse::ok(sample_expiries_json(&["2026-12-03"])),
        MockHttpResponse::ok(sample_option_chain_json(
            25365.50,
            "2026-12-03",
            "25350",
            "NIFTY-25350-CE",
            130.0,
            131.0,
            130.5,
            "NIFTY-25350-PE",
            110.0,
            111.0,
            110.5,
        )),
    ])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let snap = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 11, 0, 0))
        .await
        .expect("fetch_snapshot");

    assert!(snap.context.contains("spot=25365.50"), "ctx={}", snap.context);
    assert!(snap.context.contains("expiry=2026-12-03"), "ctx={}", snap.context);
    assert!(snap.context.contains("atm=25350"), "ctx={}", snap.context);
    assert!(snap.context.contains("pcr=1.20"), "ctx={}", snap.context);
}

#[tokio::test]
async fn t1_f5_expiry_override_skips_expiries_endpoint() {
    let server = MockIndstocksServer::start_queued(vec![MockHttpResponse::ok(
        sample_option_chain_json(
            25500.0,
            "2026-12-10",
            "25500",
            "NIFTY-25500-CE",
            100.0,
            101.0,
            100.5,
            "NIFTY-25500-PE",
            100.0,
            101.0,
            100.5,
        ),
    )])
    .await;

    let mut settings = test_settings(Some(server.base_url()));
    settings.indstocks.expiry_override = Some("2026-12-10".to_string());
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let snap = client
        .fetch_snapshot(ist_utc(2026, 11, 27, 10, 0, 0))
        .await
        .expect("fetch_snapshot");

    assert_eq!(snap.quotes.len(), 2);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/market/option-chain");
    assert!(reqs[0].query.contains("expiry=2026-12-10"));
}

#[tokio::test]
async fn t1_f5_fetch_quotes_parses_flat_and_nested_market_depth_and_commas() {
    let server = MockIndstocksServer::start_queued(vec![MockHttpResponse::ok(
        json!({
            "status": "success",
            "data": {
                "NFO_111": {
                    "live_price": "1,250.50",
                    "market_depth": {
                        "depth": [
                            { "buy": { "price": "1,249.00" }, "sell": { "price": "1,252.00" } }
                        ]
                    }
                },
                "NFO_222": {
                    "live_price": 250.25,
                    "market_depth": {
                        "NFO_222": {
                            "depth": [
                                { "buy": { "price": 249.50 }, "sell": { "price": 251.00 } }
                            ]
                        }
                    }
                }
            }
        })
        .to_string(),
    )])
    .await;

    let settings = test_settings(Some(server.base_url()));
    let client = IndstocksClient::new(settings.indstocks, "tok".to_string());
    let now = ist_utc(2026, 11, 27, 10, 30, 0);
    let quotes = client
        .fetch_quotes(&["NFO_111".to_string(), "NFO_222".to_string()], now)
        .await
        .expect("fetch_quotes");

    assert_eq!(quotes.len(), 2);
    assert!((quotes[0].bid - 1249.0).abs() < 1e-9);
    assert!((quotes[0].ask - 1252.0).abs() < 1e-9);
    assert!((quotes[0].last - 1250.5).abs() < 1e-9);
    assert!((quotes[1].bid - 249.5).abs() < 1e-9);
    assert!((quotes[1].ask - 251.0).abs() < 1e-9);
    assert!((quotes[1].last - 250.25).abs() < 1e-9);
}

// ============================================================================
// F6: `MarketDataFeed` Trait & `Clock` / IST Market-Hours Abstraction — 5 Tier-1 Tests
// ============================================================================

#[test]
fn t1_f6_fake_clock_set_and_advance_drive_now() {
    let t0 = ist_utc(2026, 11, 27, 9, 15, 0);
    let clock = FakeClock::new(t0);
    assert_eq!(clock.now(), t0);

    clock.advance(Duration::minutes(30));
    assert_eq!(clock.now(), t0 + Duration::minutes(30));

    let t_close = ist_utc(2026, 11, 27, 15, 30, 0);
    clock.set(t_close);
    assert_eq!(clock.now(), t_close);
}

#[test]
fn t1_f6_market_hours_status_premarket_open_closed_ist() {
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 9, 0, 0)),
        MarketHoursStatus::PreMarket
    );
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 10, 30, 0)),
        MarketHoursStatus::Open
    );
    assert_eq!(
        market_hours_status(ist_utc(2026, 11, 27, 16, 0, 0)),
        MarketHoursStatus::Closed
    );
}

#[test]
fn t1_f6_to_ist_converts_utc_to_plus_0530() {
    assert_eq!(ist_offset().local_minus_utc(), 5 * 3600 + 30 * 60);
    let open_utc = ist_utc(2026, 11, 27, 9, 15, 0);
    assert_eq!(open_utc.hour(), 3);
    assert_eq!(open_utc.minute(), 45);
    assert_eq!(
        to_ist(open_utc).format("%H:%M:%S").to_string(),
        "09:15:00"
    );
    assert_eq!(
        ist_date(open_utc).format("%Y-%m-%d").to_string(),
        "2026-11-27"
    );
}

#[test]
fn t1_f6_validate_quote_at_and_ensure_fresh_at_use_injected_now() {
    let historical_now = ist_utc(2024, 6, 15, 11, 0, 0);
    let q = make_quote(
        "NIFTY-25000-CE",
        100.0,
        101.0,
        100.5,
        historical_now - Duration::seconds(10),
    );
    assert!(validate_quote_at(&q, historical_now).is_ok());
    assert!(ensure_fresh_at(&q, 15, historical_now).is_ok());
}

#[test]
fn t1_f6_risk_check_market_snapshot_at_and_broker_execute_at_use_injected_now() {
    let sim_now = ist_utc(2025, 3, 12, 10, 30, 0);
    let settings = test_settings(None);
    let q = make_quote("NIFTY-25000-CE", 100.0, 100.5, 100.2, sim_now);
    let snap = make_snapshot(sim_now, vec![q.clone()], "injected now test");
    assert!(risk::check_market_snapshot_at(&snap, 15, sim_now).is_ok());

    let mut broker = PaperBroker::new(settings.paper);
    let mut sig = buy_signal("sig-at-1", "NIFTY-25000-CE", 2, 101.0, 80.0, 90.0, 130.0);
    sig.snapshot_id = Some(snap.id);
    let fill = broker
        .execute_at(&sig, &q, &snap, 0, 0, 15, sim_now)
        .expect("execute_at");
    assert_eq!(fill.timestamp, sim_now);
    assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), 2);
}

// ============================================================================
// F7: Deterministic Tick-Level SL / Trailing SL / Target Monitoring — 5 Tier-1 Tests
// ============================================================================

#[tokio::test]
async fn t1_f7_monitor_positions_triggers_stop_loss_sell_deterministically() {
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&llm_calls);
    let backend = Arc::new(ScriptedLlmBackend::new(move |snap, pos, _idx| {
        calls_ref.fetch_add(1, Ordering::SeqCst);
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    "sl-entry-1",
                    &q.symbol,
                    2,
                    102.0,
                    85.0,
                    92.0,
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

    let (_db, _journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
    let t0 = ist_utc(2026, 11, 27, 9, 30, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, t0)], "open");
    runner.step_at(&s0, t0).await.expect("entry step");
    let calls_after_entry = llm_calls.load(Ordering::SeqCst);
    assert_eq!(runner.broker.open_position_qty("NIFTY-CE"), 2);

    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 84.0, 85.0, 84.5, t1)], "sl hit");
    let fills = runner.monitor_positions_at(&s1, t1).expect("monitor");
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].side, Side::Sell);
    assert_eq!(runner.broker.open_position_qty("NIFTY-CE"), 0);
    assert_eq!(llm_calls.load(Ordering::SeqCst), calls_after_entry);
    assert_eq!(runner.closed_trades()[0].exit_reason, "stop_loss");
}

#[tokio::test]
async fn t1_f7_monitor_positions_triggers_target_price_sell_deterministically() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8500,
                signal: Some(buy_signal(
                    "tgt-entry-1",
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

    let (_db, _journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
    let t0 = ist_utc(2026, 11, 27, 9, 45, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");

    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 131.0, 132.0, 131.5, t1)], "target");
    let fills = runner.monitor_positions_at(&s1, t1).expect("monitor");
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].side, Side::Sell);
    assert_eq!(runner.broker.open_positions_count(), 0);
    assert!(runner.broker.realized_pnl_paise() > 0);
    assert_eq!(runner.closed_trades()[0].exit_reason, "target_hit");
}

#[tokio::test]
async fn t1_f7_trailing_stop_ratchets_upward_on_new_high_and_triggers_on_pullback() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            // entry_price=100.0, tsl=90.0 -> trailing_distance = 10.0
            TraderProposal {
                action: "BUY".into(),
                rationale: "tsl test".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    "tsl-entry-1",
                    &q.symbol,
                    3,
                    101.0,
                    75.0,
                    91.0,
                    160.0,
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

    // Rally to 130.0 -> TSL ratchets to 130.0 - (101.0 - 91.0) = 120.0
    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 129.0, 131.0, 130.0, t1)], "rally");
    assert!(runner.monitor_positions_at(&s1, t1).unwrap().is_empty());
    let ctx = runner.active_position_context(&s1).unwrap();
    assert!((ctx.trailing_stop_loss_price - 120.0).abs() < 1e-6);

    // Pullback to 119.5 -> triggers ratcheted TSL
    let t2 = t1 + Duration::seconds(5);
    let s2 = make_snapshot(t2, vec![make_quote("NIFTY-CE", 119.5, 120.5, 120.0, t2)], "pullback");
    let fills = runner.monitor_positions_at(&s2, t2).expect("tsl exit");
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].side, Side::Sell);
    assert_eq!(runner.closed_trades()[0].exit_reason, "trailing_stop_loss");
    assert!(runner.broker.realized_pnl_paise() > 0);
}

#[tokio::test]
async fn t1_f7_tick_exit_records_fill_and_reflection_and_clears_tracked_plan() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    "refl-entry-1",
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

    let (_db, journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");
    assert_eq!(journal.fill_count().unwrap(), 1);

    let t1 = t0 + Duration::seconds(5);
    let s1 = make_snapshot(t1, vec![make_quote("NIFTY-CE", 132.0, 133.0, 132.5, t1)], "target");
    let fills = runner.monitor_positions_at(&s1, t1).expect("exit");
    assert_eq!(fills.len(), 1);
    assert_eq!(journal.fill_count().unwrap(), 2);
    assert!(runner.active_position_context(&s1).is_none());

    let flushed = runner.flush_reflections_at(t1).await.expect("flush reflections");
    assert_eq!(flushed, 1);
    assert_eq!(journal.reflection_count().unwrap(), 1);
}

#[tokio::test]
async fn t1_f7_ticks_inside_sl_and_target_do_not_exit() {
    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    "hold-entry-1",
                    &q.symbol,
                    2,
                    102.0,
                    80.0,
                    92.0,
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

    let (_db, _journal, mut runner) = make_runner_with_backend(test_settings(None), backend);
    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 100.5, 100.2, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");

    for offset in [5i64, 10, 15, 20] {
        let tn = t0 + Duration::seconds(offset);
        let px = 100.0 + offset as f64 * 0.5;
        let sn = make_snapshot(tn, vec![make_quote("NIFTY-CE", px, px + 1.0, px + 0.5, tn)], "mid");
        let fills = runner.monitor_positions_at(&sn, tn).expect("monitor");
        assert!(fills.is_empty());
        assert_eq!(runner.broker.open_position_qty("NIFTY-CE"), 2);
    }
}

// ============================================================================
// F8: Continuous Market-Hours `paper` Session (09:15–15:30 IST) — 5 Tier-1 Tests
// ============================================================================

#[tokio::test]
async fn t1_f8_session_runs_multiple_ticks_across_market_hours_before_exiting() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 1800; // 30m ticks -> ~12 polls across 09:15..15:30
    settings.analysis.bar_minutes = 120;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 15, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-25350-CE", 100.0, 101.0, 100.5, now)],
            "steady open",
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

    assert!(
        feed.call_count() >= 10,
        "expected continuous multi-tick loop, got {} calls",
        feed.call_count()
    );
    assert_eq!(market_hours_status(clock.now()), MarketHoursStatus::Closed);
    assert_eq!(summary.open_positions_remaining, 0);
}

#[tokio::test]
async fn t1_f8_session_waits_through_premarket_until_0915_ist() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 3600;
    settings.analysis.bar_minutes = 120;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 9, 10, 0));
    let first_fetch_time = Arc::new(std::sync::Mutex::new(None));
    let first_fetch_ref = Arc::clone(&first_fetch_time);

    let feed = ScriptedFeed::new(move |_idx, now| {
        let mut guard = first_fetch_ref.lock().unwrap();
        if guard.is_none() {
            *guard = Some(now);
        }
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-25350-CE", 100.0, 101.0, 100.5, now)],
            "open",
        ))
    });

    let _summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("session completes");

    let first = first_fetch_time
        .lock()
        .unwrap()
        .expect("feed called once open");
    assert_eq!(market_hours_status(first), MarketHoursStatus::Open);
    assert_eq!(to_ist(first).format("%H:%M:%S").to_string(), "09:15:00");
}

#[tokio::test]
async fn t1_f8_session_runs_fast_tick_monitoring_between_slower_analysis_bars() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60; // 1m ticks
    settings.analysis.bar_minutes = 30; // 30m analysis bar

    let backend = Arc::new(ScriptedLlmBackend::new(|snap, pos, _| {
        if pos.is_none() {
            let q = &snap.quotes[0];
            TraderProposal {
                action: "BUY".into(),
                rationale: "enter".into(),
                confidence_bps: 8000,
                signal: Some(buy_signal(
                    &format!("cadence-{}", snap.id),
                    &q.symbol,
                    1,
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

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 0, 0));
    let feed = ScriptedFeed::new(|idx, now| {
        // Tick 2 hits target 130.0 well before the 30-min bar elapses
        let px = if idx == 2 { 132.0 } else { 100.0 };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", px, px + 1.0, px + 0.5, now)],
            "cadence test",
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

    // 15:00..15:30 at 1m poll = ~30 ticks, only 1 analysis cycle (30m bar)!
    assert!(feed.call_count() >= 25);
    assert_eq!(backend_ref.trader_call_count(), 1);
    assert!(summary
        .closed_trades
        .iter()
        .any(|t| t.exit_reason == "target_hit"));
}

#[tokio::test]
async fn t1_f8_session_respects_shutdown_signal_and_exits_cleanly() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 10, 0, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, now)],
            "open",
        ))
    });

    let notify = Arc::new(Notify::new());
    let notify_sleeper = Arc::clone(&notify);
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticks_c = Arc::clone(&ticks);
    let clk_c = clock.clone();

    let sleeper = move |d: std::time::Duration| {
        let clk = clk_c.clone();
        let n = Arc::clone(&notify_sleeper);
        let tc = Arc::clone(&ticks_c);
        async move {
            clk.advance(Duration::from_std(d).unwrap());
            if tc.fetch_add(1, Ordering::SeqCst) == 1 {
                n.notify_one();
            }
        }
    };

    let summary = run_paper_session(&mut runner, &feed, &clock, sleeper, notify.notified())
        .await
        .expect("graceful shutdown returns Ok");
    assert_eq!(summary.open_positions_remaining, 0);
    assert!(to_ist(clock.now()).hour() < 15);
}

#[tokio::test]
async fn t1_f8_session_started_when_already_closed_exits_cleanly() {
    let settings = test_settings(None);
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);
    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 35, 0));
    let feed = ScriptedFeed::new(|_idx, _now| unreachable!("feed must not be polled when Closed"));

    let summary = run_paper_session(
        &mut runner,
        &feed,
        &clock,
        clock_sleeper(clock.clone()),
        std::future::pending::<()>(),
    )
    .await
    .expect("closed exit");

    assert_eq!(feed.call_count(), 0);
    assert_eq!(summary.total_trades, 0);
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.date_ist, "2026-11-27");
}

// ============================================================================
// F9: 15:30 IST EOD Square-Off, Daily Summary & Clean Exit — 5 Tier-1 Tests
// ============================================================================

#[tokio::test]
async fn t1_f9_square_off_all_closes_open_position_at_closing_price() {
    let (_db, _journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 14, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");
    assert_eq!(runner.broker.open_positions_count(), 1);

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let s_eod = make_snapshot(t_eod, vec![make_quote("NIFTY-CE", 118.0, 119.0, 118.5, t_eod)], "close");
    let fills = runner.square_off_all(&s_eod, t_eod).expect("square_off_all");

    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].side, Side::Sell);
    assert_eq!(runner.broker.open_positions_count(), 0);
    assert!(runner.broker.realized_pnl_paise() > 0);
}

#[tokio::test]
async fn t1_f9_session_auto_squares_off_at_1530_ist() {
    let mut settings = test_settings(None);
    settings.indstocks.poll_interval_seconds = 60;
    settings.analysis.bar_minutes = 60;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let clock = FakeClock::new(ist_utc(2026, 11, 27, 15, 28, 0));
    let feed = ScriptedFeed::new(|_idx, now| {
        let px = if market_hours_status(now) == MarketHoursStatus::Closed {
            115.0
        } else {
            100.0
        };
        Ok(make_snapshot(
            now,
            vec![make_quote("NIFTY-25350-CE", px, px + 1.0, px + 0.5, now)],
            "eod test",
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

    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.open_positions_remaining, 0);
    assert_eq!(summary.closed_trades.len(), 1);
    assert_eq!(summary.closed_trades[0].exit_reason, "eod_square_off");
}

#[tokio::test]
async fn t1_f9_daily_summary_reports_pnl_trades_and_cash() {
    let settings = test_settings(None);
    let expected_start = settings.paper.starting_capital_paise;
    let (_db, _journal, mut runner) = make_runner_deterministic(settings);

    let t0 = ist_utc(2026, 11, 27, 10, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let s_eod = make_snapshot(t_eod, vec![make_quote("NIFTY-CE", 120.0, 121.0, 120.5, t_eod)], "close");
    runner.square_off_all(&s_eod, t_eod).expect("square off");

    let summary = runner.daily_summary(t_eod);
    assert_eq!(summary.date_ist, "2026-11-27");
    assert_eq!(summary.starting_cash_paise, expected_start);
    assert_eq!(summary.ending_cash_paise, runner.broker.cash_paise);
    assert_eq!(summary.realized_pnl_paise, runner.broker.realized_pnl_paise());
    assert_eq!(summary.total_trades, 1);
    assert_eq!(summary.closed_trades.len(), 1);
    assert_eq!(summary.eod_squared_off, 1);
    assert_eq!(summary.open_positions_remaining, 0);
}

#[tokio::test]
async fn t1_f9_daily_summary_display_formats_readable_report() {
    let (_db, _journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 14, 0, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-25400-PE", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let s_eod = make_snapshot(t_eod, vec![make_quote("NIFTY-25400-PE", 112.0, 113.0, 112.5, t_eod)], "close");
    runner.square_off_all(&s_eod, t_eod).expect("square off");

    let summary = runner.daily_summary(t_eod);
    let rendered = format!("{summary}");
    assert_eq!(rendered, summary.format_summary());
    assert!(rendered.contains("2026-11-27 IST"));
    assert!(rendered.contains("Realized P&L:"));
    assert!(rendered.contains("Trades Executed (Closed): 1"));
    assert!(rendered.contains("NIFTY-25400-PE"));
    assert!(rendered.contains("eod_square_off"));
}

#[tokio::test]
async fn t1_f9_eod_square_off_journals_sell_fill_and_reflection() {
    let (_db, journal, mut runner) = make_runner_deterministic(test_settings(None));
    let t0 = ist_utc(2026, 11, 27, 14, 50, 0);
    let s0 = make_snapshot(t0, vec![make_quote("NIFTY-CE", 100.0, 101.0, 100.5, t0)], "entry");
    runner.step_at(&s0, t0).await.expect("entry");
    assert_eq!(journal.fill_count().unwrap(), 1);
    assert_eq!(journal.reflection_count().unwrap(), 0);

    let t_eod = ist_utc(2026, 11, 27, 15, 30, 0);
    let s_eod = make_snapshot(t_eod, vec![make_quote("NIFTY-CE", 108.0, 109.0, 108.5, t_eod)], "close");
    runner.square_off_all(&s_eod, t_eod).expect("square off");
    let flushed = runner.flush_reflections_at(t_eod).await.expect("flush");

    assert_eq!(flushed, 1);
    assert_eq!(journal.fill_count().unwrap(), 2);
    assert_eq!(journal.reflection_count().unwrap(), 1);
}

// ============================================================================
// F10: Daily Manual Token Refresh Workflow & Cross-Platform Compatibility — 5 Tier-1 Tests
// ============================================================================

#[test]
fn t1_f10_update_scripts_exist_for_bash_and_powershell() {
    let sh = repo_root().join("scripts").join("update-indstocks-token.sh");
    let ps1 = repo_root().join("scripts").join("update-indstocks-token.ps1");
    assert!(sh.exists(), "missing update-indstocks-token.sh");
    assert!(ps1.exists(), "missing update-indstocks-token.ps1");

    let sh_txt = fs::read_to_string(&sh).expect("read .sh");
    let ps1_txt = fs::read_to_string(&ps1).expect("read .ps1");
    assert!(sh_txt.contains("INDSTOCKS_ACCESS_TOKEN="));
    assert!(sh_txt.contains(".env"));
    assert!(ps1_txt.contains("INDSTOCKS_ACCESS_TOKEN="));
    assert!(ps1_txt.contains(".env"));
}

#[test]
fn t1_f10_ps1_script_parses_cleanly_in_powershell() {
    let ps1_path = repo_root().join("scripts").join("update-indstocks-token.ps1");
    let script = format!(
        "$errs = $null; [void][System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$errs); if ($errs.Count -gt 0) {{ $errs | ForEach-Object {{ Write-Error $_.Message }}; exit 1 }}",
        ps1_path.display()
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("run powershell AST check");
    assert!(
        output.status.success(),
        "PowerShell parse failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn t1_f10_env_file_without_bom_loads_into_dotenvy_preserving_gemini_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env_path = dir.path().join(".env");
    // UTF-8 without BOM matching update-indstocks-token.{sh,ps1} output
    let content = "GEMINI_API_KEY=gemini-key-preserved\nINDSTOCKS_ACCESS_TOKEN=daily-tok-v2\n";
    fs::write(&env_path, content.as_bytes()).expect("write .env");

    let pairs: BTreeMap<String, String> = dotenvy::from_path_iter(&env_path)
        .expect("parse .env")
        .map(|res| res.expect("valid env pair"))
        .collect();
    assert_eq!(
        pairs.get("GEMINI_API_KEY").map(String::as_str),
        Some("gemini-key-preserved")
    );
    assert_eq!(
        pairs.get("INDSTOCKS_ACCESS_TOKEN").map(String::as_str),
        Some("daily-tok-v2")
    );
}

#[test]
fn t1_f10_cli_paper_fails_fast_when_token_missing() {
    let dir = tempfile::tempdir().expect("isolated cwd");
    let mut settings = load_repo_settings();
    settings.indstocks.market_data_enabled = true;
    settings.llm.use_offline_deterministic = true;
    write_isolated_settings(dir.path(), &settings);

    let output = Command::new(bin_exe())
        .current_dir(dir.path())
        .args(["paper", "--offline"])
        .env_remove("INDSTOCKS_ACCESS_TOKEN")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("spawn nifty-paper-agent paper");

    assert!(!output.status.success(), "expected non-zero exit when token missing");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("INDSTOCKS_ACCESS_TOKEN"),
        "output={combined}"
    );
    assert!(
        combined.contains("update-indstocks-token"),
        "output={combined}"
    );
}

#[test]
fn t1_f10_cli_paper_demo_runs_and_exits_zero_without_token() {
    let dir = tempfile::tempdir().expect("isolated cwd");
    let mut settings = load_repo_settings();
    settings.llm.use_offline_deterministic = true;
    write_isolated_settings(dir.path(), &settings);

    let output = Command::new(bin_exe())
        .current_dir(dir.path())
        .args(["paper", "--demo", "--offline"])
        .env_remove("INDSTOCKS_ACCESS_TOKEN")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("spawn nifty-paper-agent paper --demo --offline");

    assert!(
        output.status.success(),
        "demo mode failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}
